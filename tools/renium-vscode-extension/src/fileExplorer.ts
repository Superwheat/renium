import * as childProcess from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";

import { GitViewActions, GitViewState } from "./gitView";
import { InstanceSorter } from "./instanceSorter";
import { getPropertiesHtml } from "./propertiesHtml";
import {
  DecodeResult,
  MAX_RBSYNC_DROPPED_BYTES,
  decodeRbsyncBytes,
  decodeRbsyncToTree,
} from "./rbsyncDecode";
import { ROBLOX_CLASS_NAMES } from "./robloxClasses";
import { DEFAULT_SYNC_SERVICES } from "./serviceDefaults";
import { isScriptClass } from "./utils";

const SETTINGS_FILE_NAME = "__roblox_sync_settings.renium";
const LEGACY_SETTINGS_FILE_NAME = "__roblox_sync_settings.rbsync";
const MAX_RBSYNC_DROPPED_BASE64_CHARS = 4 * Math.ceil(MAX_RBSYNC_DROPPED_BYTES / 3);
const RENIUM_ACTIVITY_VISIBLE_STATE_KEY = "renium.activityVisible";
const RUST_CLI_BINARY = process.platform === "win32" ? "renium.exe" : "renium";
const DEFAULT_RUST_CLI_RELATIVE_PATH = RUST_CLI_BINARY;
const EXPLORER_RUST_CLI_FALLBACK_RELATIVE_PATHS = [
  DEFAULT_RUST_CLI_RELATIVE_PATH,
  `bin/${RUST_CLI_BINARY}`,
  `tools/renium/target/release/${RUST_CLI_BINARY}`,
  `tools/renium/target-pi-release/release/${RUST_CLI_BINARY}`,
  `tools/renium/target-drop-release/release/${RUST_CLI_BINARY}`,
  `tools/renium/target-rename-release/release/${RUST_CLI_BINARY}`,
  `tools/renium/target-resave-release/release/${RUST_CLI_BINARY}`,
  `tools/renium/target/debug/${RUST_CLI_BINARY}`,
  `tools/renium/target-pi-release/debug/${RUST_CLI_BINARY}`,
  `tools/renium/target-drop-release/debug/${RUST_CLI_BINARY}`,
  `tools/renium/target-rename-release/debug/${RUST_CLI_BINARY}`,
  `tools/renium/target-resave-release/debug/${RUST_CLI_BINARY}`,
];
const EXPLORER_REQUIRED_CLI_COMMANDS = [
  "bytecode-explorer-batch",
  "bytecode-set-property",
  "bytecode-add-instance",
  "bytecode-clone-instance",
  "bytecode-move-instance",
  "bytecode-remove-instance",
  "bytecode-export-model",
  "bytecode-import-model",
];
const explorerCliHelpCache = new Map<string, { mtimeMs: number; helpText?: string }>();
let packageDragDebugOutput: vscode.OutputChannel | undefined;
const PACKAGE_DRAG_DEBUG_ENABLED = process.env.RENIUM_PACKAGE_DRAG_DEBUG === "1";

export function logPackageDragDebug(message: string): void {
  if (!PACKAGE_DRAG_DEBUG_ENABLED) {
    return;
  }
  if (!packageDragDebugOutput) {
    packageDragDebugOutput = vscode.window.createOutputChannel("Renium Package Drag");
  }
  packageDragDebugOutput.appendLine(`[${new Date().toISOString()}] ${message}`);
}

const PROTECTED_STARTER_PLAYER_CONTAINERS = new Set(["StarterCharacterScripts", "StarterPlayerScripts"]);
const MODEL_PIVOT_CLASSES = new Set(["Model", "WorldModel", "Workspace"]);
const WORKSPACE_HIDDEN_STUDIO_PROPERTIES = new Set([
  "AirTurbulenceIntensity",
  "CurrentCamera",
  "LevelOfDetail",
  "ModelStreamingMode",
  "Origin",
  "Pivot Offset",
  "Scale",
  "StreamingEnabledAlias",
]);
const WORKSPACE_VISIBLE_NON_SERIALIZED_PROPERTIES = new Set(["InsertPoint"]);
const WORKSPACE_VISIBLE_SERVICE_REF_PROPERTIES = new Set(["PrimaryPart"]);
const WORKSPACE_SERVER_AUTHORITY_PROPERTIES = new Set([
  "AuthorityMode",
  "NextGenerationReplication",
  "PlayerScriptsUseInputActionSystem",
  "SignalBehavior",
  "UseFixedSimulation",
]);

type ExplorerConfig = {
  projectRoot: string;
  rustCliPath: string;
  services: string[];
};

type ViewVisibilityHandler = (viewType: string, visible: boolean) => void;

type CliServiceNode = {
  id?: string;
  index: number;
  settingsId: string;
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

type CliServiceDump = {
  service: string;
  settingsFile: string;
  rootIds: string[];
  nodes: CliServiceNode[];
};

type CliSearchDump = CliServiceDump & {
  matchIds: string[];
  visibleIds: string[];
};

type CliExplorerCounts = {
  service: string;
  settingsFile: string;
  rootChildren: number;
  descendants: number;
  instances: number;
};

type CliChildrenDump = {
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

type CliBatchOp = Record<string, unknown>;

type CliCloneInstanceResult = {
  rootSettingsId?: string;
  settingsIds?: string[];
  sourceCopies?: Array<{ from?: string; to?: string }>;
};

type CliMoveInstanceResult = CliCloneInstanceResult & {
  removedSourcePaths?: string[];
  sourceSettingsFile?: string;
  targetSettingsFile?: string;
};

type CliExportModelResult = {
  ok?: boolean;
  output?: string;
  format?: string;
  rootSettingsIds?: string[];
  instances?: number;
};

type CliImportModelResult = {
  ok?: boolean;
  rootSettingsIds?: string[];
  settingsIds?: string[];
  sourceWrites?: Array<{ settingsId?: string; path?: string }>;
};

type CliRemoveInstanceResult = {
  ok?: boolean;
  removedIndexes?: number[];
  removedSourcePaths?: string[];
};

type CliDesyncPackageLinkResult = {
  ok?: boolean;
  removedPackageLinks?: Array<{
    settingsId?: string;
    name?: string;
    className?: string;
    pathSegments?: string[];
  }>;
};

type RobloxModelFormat = "rbxm" | "rbxmx";

export type FileExplorerNodeKind = "service" | "instance";

export type FileExplorerNode = {
  id: string;
  treeId: string;
  kind: FileExplorerNodeKind;
  service: string;
  name: string;
  className: string;
  settingsId?: string;
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

type CommandRunResult = {
  code: number;
  stdout: string;
  stderr: string;
};

function readonlyScriptFileName(node: FileExplorerNode): string {
  const name = String(node.name || "Script").replace(/[<>:"/\\|?*\x00-\x1f]/g, "_");
  if (/\.(lua|luau)$/i.test(name)) {
    return name;
  }
  switch (node.className) {
    case "Script":
      return `${name}.server.luau`;
    case "LocalScript":
      return `${name}.client.luau`;
    default:
      return `${name}.luau`;
  }
}

class ReadonlyExplorerScriptContentProvider implements vscode.TextDocumentContentProvider, vscode.Disposable {
  private readonly contents = new Map<string, string>();
  private readonly changeEmitter = new vscode.EventEmitter<vscode.Uri>();
  public readonly onDidChange = this.changeEmitter.event;

  public provideTextDocumentContent(uri: vscode.Uri): string {
    return this.contents.get(uri.toString()) ?? "";
  }

  public uriFor(node: FileExplorerNode, sourcePath: string, content: string): vscode.Uri {
    const fileName = readonlyScriptFileName(node);
    const packageName = node.service.replace(/[<>:"/\\|?*\x00-\x1f]/g, "_") || "Renium";
    const uri = vscode.Uri.from({
      scheme: "renium-readonly-script",
      authority: "preview",
      path: `/${encodeURIComponent(packageName)}/${encodeURIComponent(fileName)}`,
      query: `v=${Date.now().toString(36)}${Math.random().toString(36).slice(2)}`,
    });
    this.contents.set(uri.toString(), content);
    this.changeEmitter.fire(uri);
    return uri;
  }

  public dispose(): void {
    this.contents.clear();
    this.changeEmitter.dispose();
  }
}

type SearchLoadProgress = {
  loaded: number;
  total: number;
  service: string;
};

type RbxDomProperty = {
  Name?: string;
  MemberType?: string;
  Scriptability?: string;
  Security?: {
    Read?: string;
    Write?: string;
  };
  DataType?: {
    Value?: string;
    Enum?: string;
  };
  ValueType?: {
    Name?: string;
    Category?: string;
  };
  Category?: string;
  Tags?: string[];
  Kind?: unknown;
};

type RbxDomClass = {
  Name?: string;
  Superclass?: string;
  Tags?: string[];
  Members?: RbxDomProperty[];
  Properties?: Record<string, RbxDomProperty>;
  DefaultProperties?: Record<string, unknown>;
};

type RbxDomDatabase = {
  Classes?: Record<string, RbxDomClass>;
  Enums?: Record<string, { items?: Record<string, number> }>;
};

type GeneratedRobloxPropertyInfo = {
  type?: string;
  category?: string;
  displayName?: string;
  order?: number;
  writable?: boolean;
  visible?: boolean;
  declaringClass?: string;
  enumItems?: string[];
  uiMinimum?: number;
  uiMaximum?: number;
  uiNumTicks?: number;
  sliderScaling?: string;
};

type GeneratedRobloxProperties = {
  version?: number;
  classes?: Record<string, Record<string, GeneratedRobloxPropertyInfo>>;
};

type PropertyRow = {
  name: string;
  displayName?: string;
  value: unknown;
  readonly: boolean;
  defaulted: boolean;
  category: string;
  order: number;
  dataType?: string;
  enumItems?: string[];
  uiMinimum?: number;
  uiMaximum?: number;
  uiNumTicks?: number;
  sliderScaling?: string;
};

type PropertyTemplate = Omit<PropertyRow, "value" | "defaulted"> & {
  defaultValue: unknown;
};

type VerdePropertyInfo = {
  name: string;
  displayName?: string;
  type: string;
  value: unknown;
  category: string;
  layoutOrder?: number;
  isEnum?: boolean;
  enumValues?: Array<{ name: string; value: number }>;
  displayValue?: string;
  isInstanceReference?: boolean;
  referencedInstanceId?: string;
  referencedInstanceName?: string;
  referencedInstanceClass?: string;
  isReadOnly?: boolean;
  uiMinimum?: number;
  uiMaximum?: number;
  uiNumTicks?: number;
  sliderScaling?: string;
};

type VerdeAttributeInfo = {
  name: string;
  type: string;
  value: unknown;
};

type VerdePropertiesData = {
  properties: VerdePropertyInfo[];
  tags: string[];
  attributes: VerdeAttributeInfo[];
};

type PackagePropertiesPayloadNode = {
  settingsId?: string;
  name?: string;
  className?: string;
  parentId?: string;
  pathSegments?: string[];
  properties?: Record<string, unknown>;
  attributes?: Record<string, unknown>;
};

type PackagePropertiesPayload = {
  packageId?: string;
  packageName?: string;
  source?: string;
  sourcePath?: string;
  rootClass?: string | null;
  rootName?: string | null;
  node?: PackagePropertiesPayloadNode;
};

type PropertiesUpdateMessage = {
  type: "updateProperties";
  properties: VerdePropertiesData;
  nodeName?: string;
  nodeClassName?: string;
  title?: string;
  readOnly?: boolean;
};

function cleanPropertyText(value: unknown): string {
  if (value === null || value === undefined) {
    return "";
  }
  return String(value).trim();
}

function sanitizePathSegments(value: unknown): string[] {
  if (!Array.isArray(value)) {
    return [];
  }
  return value.map((segment) => cleanPropertyText(segment)).filter((segment) => segment.length > 0);
}

type EditorHistoryManifest = {
  version?: number;
  createdUnixMs?: number;
  service?: string;
  sourcePath?: string;
  settingsId?: string;
  pathSegments?: string[];
  className?: string;
  propertyName?: string;
  propertyLabel?: string;
  settingsBackup?: string;
  sourceBackup?: string;
};

type ExplorerHistoryEntry = {
  id: string;
  service: string;
  className: string;
  settingsId?: string;
  sourcePath?: string;
  pathSegments: string[];
  propertyName?: string;
  propertyLabel?: string;
  createdUnixMs: number;
  createdLabel: string;
  targetLabel: string;
  hasSourceBackup: boolean;
  hasSettingsBackup: boolean;
};

type ExplorerHistoryTarget = ExplorerHistoryEntry & {
  openId: string;
  restoreId: string;
  editCount: number;
  firstCreatedUnixMs: number;
  lastCreatedUnixMs: number;
  timeLabel: string;
};

type ExplorerHistoryGroup = {
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

type PropertyEditHistoryItem = {
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

type CloneSnapshot = {
  settingsId?: string;
  index?: number;
  pathSegments: string[];
  name: string;
  className: string;
  properties: Record<string, unknown>;
  attributes: Record<string, unknown>;
  sourcePath?: string;
  children: CloneSnapshot[];
};

type CloneIdentitySet = {
  settingsIds: Set<string>;
  indices: Set<number>;
  pathKeys: Set<string>;
};

type CloneTargetMap = {
  bySettingsId: Map<string, FileExplorerNode>;
  byIndex: Map<number, FileExplorerNode>;
  byPathKey: Map<string, FileExplorerNode>;
};

let rbxDomDatabaseCache: RbxDomDatabase | undefined;
let generatedRobloxPropertiesCache: GeneratedRobloxProperties | undefined;
const propertyTemplateCache = new Map<string, PropertyTemplate[]>();
const scriptDisabledClasses = new Set(["Script", "LocalScript"]);
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
const valueInstanceFallbackTypes: Record<string, string> = {
  BinaryStringValue: "BinaryString",
};

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

function workspaceRoot(): string {
  const folder = vscode.workspace.workspaceFolders?.[0];
  if (!folder) {
    throw new Error("Open a workspace folder before using Renium.");
  }
  return folder.uri.fsPath;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function isNoMatchingInstanceError(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return /no matched instance|no matching instance|instance not found/i.test(message);
}

function isSettingsLockTimeout(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return /timed out waiting for settings file lock/i.test(message);
}

function resolveConfigPath(raw: string, root: string): string {
  const replaced = raw
    .replaceAll("${workspaceFolder}", root)
    .replaceAll("${userHome}", os.homedir());
  return path.isAbsolute(replaced) ? path.normalize(replaced) : path.normalize(path.join(root, replaced));
}

function explorerCliHelpText(cliPath: string): string | undefined {
  try {
    const stat = fs.statSync(cliPath);
    const cached = explorerCliHelpCache.get(cliPath);
    if (cached && cached.mtimeMs === stat.mtimeMs) {
      return cached.helpText;
    }
    const result = childProcess.spawnSync(cliPath, ["--help"], {
      cwd: path.dirname(cliPath),
      encoding: "utf8",
      shell: false,
      windowsHide: true,
    });
    const helpText = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
    explorerCliHelpCache.set(cliPath, { mtimeMs: stat.mtimeMs, helpText });
    return helpText;
  } catch {
    return undefined;
  }
}

function explorerCliSupportsRequiredCommands(cliPath: string): boolean {
  if (!fs.existsSync(cliPath)) {
    return false;
  }
  const helpText = explorerCliHelpText(cliPath);
  return helpText !== undefined && EXPLORER_REQUIRED_CLI_COMMANDS.every((command) => helpText.includes(command));
}

function explorerCliSupportsCommand(cliPath: string, command: string): boolean {
  if (!fs.existsSync(cliPath)) {
    return false;
  }
  try {
    const result = childProcess.spawnSync(cliPath, [command, "--help"], {
      cwd: path.dirname(cliPath),
      encoding: "utf8",
      shell: false,
      timeout: 2000,
      windowsHide: true,
    });
    return !result.error && result.status === 0;
  } catch {
    return false;
  }
}

function resolveExplorerRustCliPath(root: string, configuredPath: string): string {
  const candidates = [
    configuredPath,
    ...EXPLORER_RUST_CLI_FALLBACK_RELATIVE_PATHS.map((relativePath) => resolveConfigPath(relativePath, root)),
  ].map((candidate) => path.normalize(candidate));
  const uniqueCandidates = Array.from(new Set(candidates));

  if (explorerCliSupportsRequiredCommands(configuredPath)) {
    return configuredPath;
  }

  const supportedCandidates = uniqueCandidates.filter((candidate) => explorerCliSupportsRequiredCommands(candidate));
  if (supportedCandidates.length === 0) {
    return configuredPath;
  }

  supportedCandidates.sort((left, right) => {
    const leftMtime = fs.statSync(left).mtimeMs;
    const rightMtime = fs.statSync(right).mtimeMs;
    return rightMtime - leftMtime;
  });
  return supportedCandidates[0];
}

function resolveExplorerRustCliPathForCommand(root: string, configuredPath: string, command: string): string {
  const candidates = [
    configuredPath,
    ...EXPLORER_RUST_CLI_FALLBACK_RELATIVE_PATHS.map((relativePath) => resolveConfigPath(relativePath, root)),
  ].map((candidate) => path.normalize(candidate));
  const uniqueCandidates = Array.from(new Set(candidates));
  const supportedCandidates = uniqueCandidates.filter(
    (candidate) => explorerCliSupportsRequiredCommands(candidate) && explorerCliSupportsCommand(candidate, command),
  );
  if (supportedCandidates.length === 0) {
    return resolveExplorerRustCliPath(root, configuredPath);
  }
  supportedCandidates.sort((left, right) => {
    const leftMtime = fs.statSync(left).mtimeMs;
    const rightMtime = fs.statSync(right).mtimeMs;
    return rightMtime - leftMtime;
  });
  return supportedCandidates[0];
}

function getExplorerConfig(): ExplorerConfig {
  const root = workspaceRoot();
  const cfg = vscode.workspace.getConfiguration("renium");
  const projectRoot = resolveConfigPath(cfg.get<string>("projectRoot", "${workspaceFolder}"), root);
  const configuredRustCliPath = resolveConfigPath(
    cfg.get<string>("rustCliPath", "${workspaceFolder}/" + DEFAULT_RUST_CLI_RELATIVE_PATH),
    root,
  );
  const rustCliPath = resolveExplorerRustCliPath(root, configuredRustCliPath);
  const servicesRaw = cfg.get<string[]>("services", [...DEFAULT_SYNC_SERVICES]);
  const services = Array.isArray(servicesRaw)
    ? servicesRaw.map((value) => String(value).trim()).filter((value) => value.length > 0)
    : [...DEFAULT_SYNC_SERVICES];
  return { projectRoot, rustCliPath, services };
}

function srcRoot(config: ExplorerConfig): string {
  return path.join(config.projectRoot, "src");
}

function settingsFileForService(config: ExplorerConfig, service: string): string {
  const serviceDir = path.join(srcRoot(config), service);
  const canonical = path.join(serviceDir, SETTINGS_FILE_NAME);
  const legacy = path.join(serviceDir, LEGACY_SETTINGS_FILE_NAME);
  return fs.existsSync(canonical) || !fs.existsSync(legacy) ? canonical : legacy;
}

function isSettingsFileName(fileName: string): boolean {
  const normalized = fileName.toLowerCase();
  return normalized === SETTINGS_FILE_NAME || normalized === LEGACY_SETTINGS_FILE_NAME;
}

function editorHistoryRoot(config: ExplorerConfig): string {
  return path.join(config.projectRoot, ".renium", "editor-history");
}

function safeHistoryComponent(value: unknown): string {
  const cleaned = String(value ?? "item")
    .trim()
    .replace(/[^A-Za-z0-9._-]+/g, "_")
    .replace(/^_+|_+$/g, "")
    .slice(0, 80);
  return cleaned || "item";
}

function safeModelFileName(name: string, format: RobloxModelFormat): string {
  return `${safeHistoryComponent(name || "Model")}.${format}`;
}

function ensureModelFileExtension(filePath: string, format: RobloxModelFormat): string {
  const expected = `.${format}`;
  const current = path.extname(filePath).toLowerCase();
  if (current === expected) {
    return filePath;
  }
  if (current === ".rbxm" || current === ".rbxmx") {
    return `${filePath.slice(0, -current.length)}${expected}`;
  }
  return `${filePath}${expected}`;
}

function robloxModelFormatFromPath(filePath: string): RobloxModelFormat | undefined {
  const extension = path.extname(filePath).toLowerCase();
  if (extension === ".rbxm" || extension === ".rbxmx") {
    return extension.slice(1) as RobloxModelFormat;
  }
  return undefined;
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

function normalizeFilesystemPathKey(filePath: string): string {
  const normalized = path.resolve(filePath).replace(/\\/g, "/");
  return process.platform === "win32" ? normalized.toLowerCase() : normalized;
}

function normalizeRobloxModelPaths(rawPaths: string[] | undefined): string[] {
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

function cloneHistoryValue(value: unknown): unknown {
  try {
    return JSON.parse(JSON.stringify(value));
  } catch {
    return value;
  }
}

function serviceFromSettingsFile(config: ExplorerConfig, filePath: string): string | undefined {
  if (!isSettingsFileName(path.basename(filePath))) {
    return undefined;
  }
  const relativePath = path.relative(srcRoot(config), filePath);
  if (!relativePath || relativePath.startsWith("..") || path.isAbsolute(relativePath)) {
    return undefined;
  }
  const [service] = relativePath.split(/[\\/]/);
  return service || undefined;
}

function normalizeId(service: string, settingsId: string): string {
  return `${service}:${settingsId}`;
}

function serviceTreeId(service: string): string {
  return `service:${service}`;
}

function isProtectedStarterPlayerContainer(node: FileExplorerNode): boolean {
  return node.kind === "instance" &&
    node.service === "StarterPlayer" &&
    node.parentTreeId === serviceTreeId("StarterPlayer") &&
    PROTECTED_STARTER_PLAYER_CONTAINERS.has(node.name);
}

function safeObject(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {};
}

function safeArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function escapeHtml(value: unknown): string {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll("\"", "&quot;");
}

function parseJsonValue(text: string): unknown {
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

function jsonValuesEqual(a: unknown, b: unknown): boolean {
  return Object.is(a, b) || jsonValueText(a) === jsonValueText(b);
}

function pathInsideRoot(root: string, candidate: string): boolean {
  const relativePath = path.relative(normalizeFilesystemPathKey(root), normalizeFilesystemPathKey(candidate));
  return relativePath === "" || (!relativePath.startsWith("..") && !path.isAbsolute(relativePath));
}

function loadRbxDomDatabase(config: ExplorerConfig): RbxDomDatabase | undefined {
  if (rbxDomDatabaseCache !== undefined) {
    return rbxDomDatabaseCache;
  }
  const databasePath = [
    path.join(config.projectRoot, "API-Dump.json"),
    path.join(config.projectRoot, "Full-API-Dump.json"),
    path.join(config.projectRoot, "tools", "plugin_ws_bridge", "rbx_dom_lua", "database.json"),
  ].find((candidate) => fs.existsSync(candidate));
  if (!databasePath) {
    rbxDomDatabaseCache = {};
    return rbxDomDatabaseCache;
  }
  try {
    rbxDomDatabaseCache = normalizeRbxDomDatabase(JSON.parse(fs.readFileSync(databasePath, "utf8")));
  } catch {
    rbxDomDatabaseCache = {};
  }
  return rbxDomDatabaseCache;
}

function loadGeneratedRobloxProperties(config: ExplorerConfig): GeneratedRobloxProperties | undefined {
  if (generatedRobloxPropertiesCache !== undefined) {
    return generatedRobloxPropertiesCache;
  }
  const extensionRoot = path.resolve(__dirname, "..");
  const metadataPath = [
    path.join(extensionRoot, "resources", "roblox-properties.generated.json"),
    path.join(config.projectRoot, "tools", "renium-vscode-extension", "resources", "roblox-properties.generated.json"),
  ].find((candidate) => fs.existsSync(candidate));
  if (!metadataPath) {
    generatedRobloxPropertiesCache = {};
    return generatedRobloxPropertiesCache;
  }
  try {
    generatedRobloxPropertiesCache = JSON.parse(fs.readFileSync(metadataPath, "utf8")) as GeneratedRobloxProperties;
  } catch {
    generatedRobloxPropertiesCache = {};
  }
  return generatedRobloxPropertiesCache;
}

function generatedPropertyInfo(
  metadata: GeneratedRobloxProperties | undefined,
  className: string,
  propertyName: string,
): GeneratedRobloxPropertyInfo | undefined {
  const byClass = metadata?.classes?.[className];
  return byClass?.[propertyName];
}

function hasGeneratedPropertyList(metadata: GeneratedRobloxProperties | undefined, className: string): boolean {
  return !!metadata?.classes?.[className];
}

function isGeneratedPropertyVisible(
  metadata: GeneratedRobloxProperties | undefined,
  className: string,
  propertyName: string,
): boolean {
  const byClass = metadata?.classes?.[className];
  return !byClass || !!byClass[propertyName];
}

function normalizeRbxDomDatabase(raw: unknown): RbxDomDatabase {
  const record = safeObject(raw);
  if (!Array.isArray(record.Classes)) {
    return record as RbxDomDatabase;
  }

  const classes: Record<string, RbxDomClass> = {};
  for (const rawClass of record.Classes) {
    const classRecord = safeObject(rawClass);
    const className = String(classRecord.Name ?? "");
    if (!className) {
      continue;
    }
    const properties: Record<string, RbxDomProperty> = {};
    for (const rawMember of safeArray(classRecord.Members)) {
      const member = safeObject(rawMember);
      if (member.MemberType !== "Property") {
        continue;
      }
      const propertyName = String(member.Name ?? "");
      if (!propertyName) {
        continue;
      }
      const valueType = safeObject(member.ValueType);
      const valueTypeName = typeof valueType.Name === "string" ? valueType.Name : undefined;
      const valueTypeCategory = typeof valueType.Category === "string" ? valueType.Category : undefined;
      properties[propertyName] = {
        Name: propertyName,
        MemberType: "Property",
        Security: safeObject(member.Security) as RbxDomProperty["Security"],
        ValueType: { Name: valueTypeName, Category: valueTypeCategory },
        DataType: rbxDomDataTypeFromApiDumpValueType(valueTypeName, valueTypeCategory),
        Category: typeof member.Category === "string" ? member.Category : undefined,
        Tags: safeArray(member.Tags).map((tag) => String(tag)),
      };
    }
    classes[className] = {
      Name: className,
      Superclass: typeof classRecord.Superclass === "string" ? classRecord.Superclass : undefined,
      Tags: safeArray(classRecord.Tags).map((tag) => String(tag)),
      Properties: properties,
      DefaultProperties: {},
    };
  }

  const enums: Record<string, { items?: Record<string, number> }> = {};
  for (const rawEnum of safeArray(record.Enums)) {
    const enumRecord = safeObject(rawEnum);
    const enumName = String(enumRecord.Name ?? "");
    if (!enumName) {
      continue;
    }
    const items: Record<string, number> = {};
    for (const rawItem of safeArray(enumRecord.Items)) {
      const item = safeObject(rawItem);
      const itemName = String(item.Name ?? "");
      const itemValue = typeof item.Value === "number" ? item.Value : Number(item.Value);
      if (itemName && Number.isFinite(itemValue)) {
        items[itemName] = itemValue;
      }
    }
    enums[enumName] = { items };
  }

  return { Classes: classes, Enums: enums };
}

function rbxDomDataTypeFromApiDumpValueType(name: string | undefined, category: string | undefined): RbxDomProperty["DataType"] {
  if (!name) {
    return undefined;
  }
  if (category === "Enum") {
    return { Enum: name.replace(/^Enum\./, "") };
  }
  if (category === "Class") {
    return { Value: "Ref" };
  }
  const primitiveMap: Record<string, string> = {
    bool: "Bool",
    boolean: "Bool",
    int: "Int32",
    int64: "Int64",
    float: "Float32",
    double: "Float64",
    string: "String",
    BinaryString: "BinaryString",
    Content: "ContentId",
  };
  return { Value: primitiveMap[name] ?? name };
}

function findRbxDomProperty(classes: Record<string, RbxDomClass>, className: string, propertyName: string): RbxDomProperty | undefined {
  const seen = new Set<string>();
  let current: string | undefined = className;
  while (current && !seen.has(current)) {
    seen.add(current);
    const classInfo: RbxDomClass | undefined = classes[current];
    const property = classInfo?.Properties?.[propertyName];
    if (property) {
      return property;
    }
    current = classInfo?.Superclass;
  }
  return fallbackValueInstanceProperty(className, propertyName);
}

function fallbackValueInstanceProperty(className: string, propertyName: string): RbxDomProperty | undefined {
  const valueType = valueInstanceFallbackTypes[className];
  if (!valueType || propertyName !== "Value") {
    return undefined;
  }
  return {
    Name: "Value",
    Scriptability: "ReadWrite",
    DataType: { Value: valueType },
    Tags: [],
  };
}

function collectRbxDomClassChain(classes: Record<string, RbxDomClass>, className: string): string[] {
  const chain: string[] = [];
  const seen = new Set<string>();
  let current: string | undefined = className;
  while (current && !seen.has(current)) {
    seen.add(current);
    const classInfo: RbxDomClass | undefined = classes[current];
    if (!classInfo) {
      break;
    }
    chain.unshift(current);
    current = classInfo.Superclass;
  }
  return chain;
}

function propertyTags(property: RbxDomProperty | undefined): Set<string> {
  return new Set(Array.isArray(property?.Tags) ? property.Tags : []);
}

function classHasDefaultProperty(classes: Record<string, RbxDomClass> | undefined, className: string, propertyName: string): boolean {
  const defaults = classes?.[className]?.DefaultProperties;
  return !!defaults && Object.prototype.hasOwnProperty.call(defaults, propertyName);
}

function isDefaultBackedStudioProperty(
  classes: Record<string, RbxDomClass> | undefined,
  className: string,
  declaringClassName: string | undefined,
  propertyName: string,
): boolean {
  return classHasDefaultProperty(classes, className, propertyName) ||
    (!!declaringClassName && classHasDefaultProperty(classes, declaringClassName, propertyName));
}

function allowsHiddenDefaultBackedStudioProperty(name: string, property: RbxDomProperty | undefined): boolean {
  if (name === "AvatarJointUpgrade_SerializedRollout") {
    return true;
  }
  const tags = propertyTags(property);
  if (!tags.has("Hidden")) {
    return false;
  }
  if (!tags.has("NotScriptable") || tags.has("NotReplicated")) {
    return false;
  }
  if (propertyDataType(property) !== "Enum.LoadCharacterLayeredClothing") {
    return false;
  }
  return !/^GameSettings/i.test(name) && !/Serialized|Rollout/i.test(name);
}

function hasBlockedStudioPropertyTag(property: RbxDomProperty | undefined, allowHiddenStudioProperty = false): boolean {
  const tags = propertyTags(property);
  return tags.has("ReadOnly") ||
    (tags.has("Hidden") && !allowHiddenStudioProperty) ||
    tags.has("Deprecated") ||
    tags.has("NotBrowsable") ||
    tags.has("WriteOnly");
}

function isSerializedStudioProperty(property: RbxDomProperty | undefined): boolean {
  const kind = property?.Kind;
  if (!kind || typeof kind !== "object" || Array.isArray(kind)) {
    return true;
  }
  const kindRecord = kind as Record<string, unknown>;
  if (kindRecord.Alias && typeof kindRecord.Alias === "object") {
    return false;
  }
  const canonical = kindRecord.Canonical;
  if (!canonical || typeof canonical !== "object" || Array.isArray(canonical)) {
    return true;
  }
  const serialization = (canonical as Record<string, unknown>).Serialization;
  return serialization !== "DoesNotSerialize";
}

function propertyCanonicalSerialization(property: RbxDomProperty | undefined): unknown {
  const kind = property?.Kind;
  if (!kind || typeof kind !== "object" || Array.isArray(kind)) {
    return undefined;
  }
  const canonical = (kind as Record<string, unknown>).Canonical;
  if (!canonical || typeof canonical !== "object" || Array.isArray(canonical)) {
    return undefined;
  }
  return (canonical as Record<string, unknown>).Serialization;
}

function propertyMigrationTarget(property: RbxDomProperty | undefined): string | undefined {
  const serialization = propertyCanonicalSerialization(property);
  if (!serialization || typeof serialization !== "object" || Array.isArray(serialization)) {
    return undefined;
  }
  const migrate = (serialization as Record<string, unknown>).Migrate;
  if (!migrate || typeof migrate !== "object" || Array.isArray(migrate)) {
    return undefined;
  }
  const to = (migrate as Record<string, unknown>).To;
  return typeof to === "string" ? to : undefined;
}

function isSupersededMigratedProperty(
  className: string,
  property: RbxDomProperty | undefined,
  classes?: Record<string, RbxDomClass>,
): boolean {
  const target = propertyMigrationTarget(property);
  if (!target || !classes) {
    return false;
  }
  const targetProperty = findRbxDomProperty(classes, className, target);
  if (!targetProperty) {
    return false;
  }
  if (propertyCanonicalSerialization(targetProperty) === "DoesNotSerialize") {
    return true;
  }
  return isWritableStudioProperty(targetProperty);
}

function isWritableStudioProperty(
  property: RbxDomProperty | undefined,
  allowHiddenStudioProperty = false,
  allowNonSerializedStudioProperty = false,
): boolean {
  if (!property) {
    return false;
  }
  if (property.MemberType && property.MemberType !== "Property") {
    return false;
  }
  if (hasBlockedStudioPropertyTag(property, allowHiddenStudioProperty)) {
    return false;
  }
  return allowNonSerializedStudioProperty || isSerializedStudioProperty(property);
}

function classHasTag(classes: Record<string, RbxDomClass> | undefined, className: string, tag: string): boolean {
  const tags = classes?.[className]?.Tags;
  return Array.isArray(tags) && tags.includes(tag);
}

function allowsNonSerializedStudioProperty(className: string, name: string): boolean {
  return (name === "WorldPivot" && MODEL_PIVOT_CLASSES.has(className)) ||
    (className === "Workspace" && WORKSPACE_VISIBLE_NON_SERIALIZED_PROPERTIES.has(name));
}

function allowsServiceRefStudioProperty(className: string, name: string): boolean {
  return className === "Workspace" && WORKSPACE_VISIBLE_SERVICE_REF_PROPERTIES.has(name);
}

function isEngineManagedStudioProperty(
  className: string,
  property: RbxDomProperty | undefined,
  classes?: Record<string, RbxDomClass>,
  propertyName?: string,
): boolean {
  const dataType = propertyDataType(property);
  return dataType === "UniqueId" ||
    dataType === "SecurityCapabilities" ||
    (dataType === "Ref" && classHasTag(classes, className, "Service") && !allowsServiceRefStudioProperty(className, propertyName ?? ""));
}

function isVisibleStudioProperty(
  className: string,
  name: string,
  property: RbxDomProperty | undefined,
  classes?: Record<string, RbxDomClass>,
  declaringClassName?: string,
): boolean {
  if (!property) {
    return false;
  }
  if (
    name === "Name" ||
    name === "ClassName" ||
    name === "Parent" ||
    name === "Sandboxed" ||
    name === "DefinesCapabilities" ||
    name === "Attributes" ||
    name === "Tags" ||
    name === "Source" ||
    name === "LinkedSource"
  ) {
    return false;
  }
  if (className === "Workspace" && WORKSPACE_HIDDEN_STUDIO_PROPERTIES.has(name)) {
    return false;
  }
  if (isEngineManagedStudioProperty(className, property, classes, name)) {
    return false;
  }
  if (isSupersededMigratedProperty(className, property, classes)) {
    return false;
  }
  const allowHidden = isDefaultBackedStudioProperty(classes, className, declaringClassName, name) &&
    allowsHiddenDefaultBackedStudioProperty(name, property);
  return isWritableStudioProperty(property, allowHidden, allowsNonSerializedStudioProperty(className, name));
}

function isMetadataPropertyName(name: string): boolean {
  return name.toLowerCase() === "name" || name.toLowerCase() === "classname" || name.toLowerCase() === "parent";
}

function isVisibleStudioPropertyForNode(
  node: FileExplorerNode,
  name: string,
  property: RbxDomProperty | undefined,
  classes?: Record<string, RbxDomClass>,
  declaringClassName?: string,
): boolean {
  if (isMetadataPropertyName(name)) {
    return false;
  }
  return isVisibleStudioProperty(node.className, name, property, classes, declaringClassName);
}

function isReadonlyStudioProperty(
  property: RbxDomProperty | undefined,
  allowDefaultBackedStudioProperty = false,
  allowNonSerializedStudioProperty = false,
): boolean {
  return !isWritableStudioProperty(property, allowDefaultBackedStudioProperty, allowNonSerializedStudioProperty);
}

function isReadonlyStudioPropertyForNode(
  node: FileExplorerNode,
  name: string,
  property: RbxDomProperty | undefined,
  classes?: Record<string, RbxDomClass>,
  declaringClassName?: string,
): boolean {
  const allowHidden = isDefaultBackedStudioProperty(classes, node.className, declaringClassName, name) &&
    allowsHiddenDefaultBackedStudioProperty(name, property);
  return isReadonlyStudioProperty(property, allowHidden, allowsNonSerializedStudioProperty(node.className, name));
}

function isScriptNodeClass(className: string): boolean {
  return className === "Script" || className === "LocalScript" || className === "ModuleScript";
}

function usesDisabledProperty(className: string): boolean {
  return scriptDisabledClasses.has(className);
}

function propertyDataType(property: RbxDomProperty | undefined): string | undefined {
  return property?.DataType?.Enum ? `Enum.${property.DataType.Enum}` : property?.DataType?.Value;
}

function propertyDisplayName(metadata: GeneratedRobloxProperties | undefined, className: string, name: string): string | undefined {
  const generated = generatedPropertyInfo(metadata, className, name);
  return generated?.displayName && generated.displayName !== name ? generated.displayName : undefined;
}

function propertyCategory(
  className: string,
  name: string,
  property: RbxDomProperty | undefined,
  metadata?: GeneratedRobloxProperties,
): string {
  const generated = generatedPropertyInfo(metadata, className, name);
  if (generated?.category) {
    return generated.category;
  }
  if (name === "Archivable") {
    return "Data";
  }
  if (className === "Workspace" && name === "SandboxedInstanceMode") {
    return "Permissions";
  }
  if (className === "Workspace" && WORKSPACE_SERVER_AUTHORITY_PROPERTIES.has(name)) {
    return "Server Authority";
  }
  if (property?.Category) {
    return property.Category;
  }
  const dataType = propertyDataType(property) ?? "";
  const lower = name.toLowerCase();
  if (className === "Lighting") {
    if (
      dataType === "Color3" ||
      lower.includes("ambient") ||
      lower.includes("brightness") ||
      lower.includes("color") ||
      lower.includes("diffuse") ||
      lower.includes("specular") ||
      lower.includes("exposure") ||
      lower.includes("fog") ||
      lower.includes("shadow") ||
      lower.includes("time") ||
      lower.includes("technology") ||
      lower.includes("lightingstyle")
    ) {
      return "Appearance";
    }
  }
  if (
    lower.includes("enabled") ||
    lower.includes("disabled") ||
    lower === "runcontext" ||
    lower.includes("autoload") ||
    lower.includes("can") ||
    lower.includes("locked") ||
    lower.includes("visible") ||
    lower.includes("active") ||
    lower.includes("selectable") ||
    lower.includes("shadows") ||
    lower.includes("quality") ||
    lower.includes("respawn")
  ) {
    return "Behavior";
  }
  if (
    lower.includes("position") ||
    lower.includes("size") ||
    lower.includes("cframe") ||
    lower.includes("orientation") ||
    lower.includes("rotation") ||
    lower.includes("pivot") ||
    lower.includes("origin") ||
    lower.includes("scale") ||
    lower.includes("offset")
  ) {
    return "Transform";
  }
  if (lower.includes("text") || lower.includes("font") || lower.includes("lineheight")) {
    return "Text";
  }
  if (lower.includes("image") || lower.includes("slice") || lower.includes("tile")) {
    return "Image";
  }
  if (lower.includes("layout") || lower.includes("padding") || lower.includes("alignment") || lower.includes("sortorder")) {
    return "Layout";
  }
  if (lower.includes("localization") || lower.includes("localize")) {
    return "Localization";
  }
  return "Data";
}

function propertyOrder(
  metadata: GeneratedRobloxProperties | undefined,
  className: string,
  name: string,
  fallbackOrder: number,
): number {
  const generatedOrder = generatedPropertyInfo(metadata, className, name)?.order;
  return typeof generatedOrder === "number" && Number.isFinite(generatedOrder) ? generatedOrder : fallbackOrder;
}

function propertyCategoryRank(category: string): number {
  const order = [
    "Data",
    "Camera",
    "Character",
    "Character Jump Settings",
    "Controls",
    "Mobile",
    "Permissions",
    "Behavior",
    "Appearance",
    "Pivot",
    "Transform",
    "Air Properties",
    "Avatar",
    "Networking",
    "Physics",
    "Pathfinding",
    "Rendering",
    "Scripting",
    "Server Authority",
    "Streaming",
    "Text",
    "Image",
    "Layout",
    "Localization",
    "Tags",
    "Attributes",
  ];
  const index = order.indexOf(category);
  return index === -1 ? order.length : index;
}

function comparePropertyRows<T extends { category: string; order: number; name: string }>(a: T, b: T): number {
  const categorySort = propertyCategoryRank(a.category) - propertyCategoryRank(b.category);
  return categorySort || a.category.localeCompare(b.category) || a.order - b.order || a.name.localeCompare(b.name);
}

function sortPropertyRows<T extends { category: string; order: number; name: string }>(rows: T[]): T[] {
  return rows.sort(comparePropertyRows);
}

function enumItemsForProperty(property: RbxDomProperty | undefined, database: RbxDomDatabase): string[] | undefined {
  const enumType = property?.DataType?.Enum;
  if (!enumType) {
    return undefined;
  }
  const items = database.Enums?.[enumType]?.items;
  if (!items) {
    return undefined;
  }
  return Object.entries(items)
    .sort((a, b) => a[1] - b[1])
    .map(([name]) => name);
}

function propertyFromGeneratedInfo(info: GeneratedRobloxPropertyInfo | undefined): RbxDomProperty | undefined {
  const type = info?.type;
  if (!type) {
    return undefined;
  }
  if (type.startsWith("Enum.")) {
    return { MemberType: "Property", DataType: { Enum: type.slice("Enum.".length) } };
  }
  return { MemberType: "Property", DataType: { Value: type } };
}

function enumItemsForGeneratedInfo(info: GeneratedRobloxPropertyInfo | undefined, property: RbxDomProperty | undefined, database: RbxDomDatabase): string[] | undefined {
  return info?.enumItems ?? enumItemsForProperty(property, database);
}

function defaultValueForDataType(dataType: string | undefined, database: RbxDomDatabase, enumItems?: string[]): unknown {
  if (dataType?.startsWith("Enum.")) {
    const enumValues = enumValuesForDataType(dataType, database, enumItems);
    const enumName = enumValues?.find((item) => item.name === "Default")?.name ?? enumValues?.[0]?.name ?? "";
    return { _type: "EnumItem", enumType: dataType, name: enumName };
  }
  switch (dataType) {
    case "Bool":
      return false;
    case "Int32":
    case "Int64":
    case "Float32":
    case "Float64":
    case "Double":
      return 0;
    case "Vector2":
      return { _type: "Vector2", x: 0, y: 0 };
    case "Vector3":
      return { _type: "Vector3", x: 0, y: 0, z: 0 };
    case "CFrame":
    case "OptionalCFrame":
      return defaultCFrameValue();
    case "Color3":
      return { _type: "Color3", r: 0, g: 0, b: 0 };
    case "BrickColor":
      return { _type: "BrickColor", number: 194 };
    case "Ref":
      return null;
    default:
      return "";
  }
}

function unwrapDefaultPropertyValue(raw: unknown, property: RbxDomProperty | undefined, database: RbxDomDatabase): unknown {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    return raw;
  }
  const entries = Object.entries(raw as Record<string, unknown>);
  if (entries.length !== 1) {
    return raw;
  }
  const [kind, value] = entries[0];
  switch (kind) {
    case "Bool":
    case "Int32":
    case "Int64":
    case "Float32":
    case "Float64":
    case "OptionalCFrame":
    case "String":
    case "ContentId":
      return value;
    case "Enum": {
      const enumType = property?.DataType?.Enum;
      if (enumType && typeof value === "number") {
        const enumItems = database.Enums?.[enumType]?.items ?? {};
        const itemName = Object.entries(enumItems).find(([, enumValue]) => enumValue === value)?.[0];
        return {
          _type: "EnumItem",
          enumType: `Enum.${enumType}`,
          name: itemName ?? String(value),
        };
      }
      return value;
    }
    case "BrickColor":
      return { _type: "BrickColor", number: value };
    case "Color3":
      if (Array.isArray(value)) {
        return { _type: "Color3", r: value[0] ?? 0, g: value[1] ?? 0, b: value[2] ?? 0 };
      }
      return value;
    case "Color3uint8":
      if (Array.isArray(value)) {
        return {
          _type: "Color3",
          r: Number(value[0] ?? 0) / 255,
          g: Number(value[1] ?? 0) / 255,
          b: Number(value[2] ?? 0) / 255,
        };
      }
      return value;
    case "Vector2":
      if (Array.isArray(value)) {
        return { _type: "Vector2", x: value[0] ?? 0, y: value[1] ?? 0 };
      }
      return value;
    case "Vector3":
      if (Array.isArray(value)) {
        return { _type: "Vector3", x: value[0] ?? 0, y: value[1] ?? 0, z: value[2] ?? 0 };
      }
      return value;
    case "UDim":
      if (Array.isArray(value)) {
        return { _type: "UDim", scale: value[0] ?? 0, offset: value[1] ?? 0 };
      }
      return value;
    case "UDim2":
      if (Array.isArray(value) && Array.isArray(value[0]) && Array.isArray(value[1])) {
        return {
          _type: "UDim2",
          xScale: value[0][0] ?? 0,
          xOffset: value[0][1] ?? 0,
          yScale: value[1][0] ?? 0,
          yOffset: value[1][1] ?? 0,
        };
      }
      return value;
    case "CFrame":
      if (value && typeof value === "object" && !Array.isArray(value)) {
        const obj = value as { position?: unknown; orientation?: unknown };
        const position = Array.isArray(obj.position) ? obj.position : [0, 0, 0];
        const orientation = Array.isArray(obj.orientation) ? obj.orientation : [[1, 0, 0], [0, 1, 0], [0, 0, 1]];
        const row0 = Array.isArray(orientation[0]) ? orientation[0] : [1, 0, 0];
        const row1 = Array.isArray(orientation[1]) ? orientation[1] : [0, 1, 0];
        const row2 = Array.isArray(orientation[2]) ? orientation[2] : [0, 0, 1];
        return {
          _type: "CFrame",
          components: [
            position[0] ?? 0,
            position[1] ?? 0,
            position[2] ?? 0,
            row0[0] ?? 1,
            row0[1] ?? 0,
            row0[2] ?? 0,
            row1[0] ?? 0,
            row1[1] ?? 1,
            row1[2] ?? 0,
            row2[0] ?? 0,
            row2[1] ?? 0,
            row2[2] ?? 1,
          ],
        };
      }
      return value;
    default:
      return raw;
  }
}

function propertyTemplatesForClass(
  className: string,
  database: RbxDomDatabase,
  classes: Record<string, RbxDomClass>,
  generatedMetadata?: GeneratedRobloxProperties,
): PropertyTemplate[] {
  const cached = propertyTemplateCache.get(className);
  if (cached) {
    return cached;
  }
  const rows = new Map<string, PropertyTemplate>();
  let nextOrder = 0;
  const pseudoNode = { className } as FileExplorerNode;
  const setTemplate = (name: string, property: RbxDomProperty | undefined, defaultValue: unknown, declaringClassName?: string): void => {
    if (hasGeneratedPropertyList(generatedMetadata, className) && !isGeneratedPropertyVisible(generatedMetadata, className, name)) {
      return;
    }
    const existing = rows.get(name);
    const generated = generatedPropertyInfo(generatedMetadata, className, name);
    const fallbackOrder = existing?.order ?? nextOrder++;
    rows.set(name, {
      name,
      displayName: existing?.displayName ?? propertyDisplayName(generatedMetadata, className, name),
      defaultValue,
      readonly: isReadonlyStudioPropertyForNode(pseudoNode, name, property, classes, declaringClassName),
      category: existing?.category ?? propertyCategory(className, name, property, generatedMetadata),
      order: propertyOrder(generatedMetadata, className, name, fallbackOrder),
      dataType: propertyDataType(property),
      enumItems: enumItemsForProperty(property, database),
      uiMinimum: existing?.uiMinimum ?? generated?.uiMinimum,
      uiMaximum: existing?.uiMaximum ?? generated?.uiMaximum,
      uiNumTicks: existing?.uiNumTicks ?? generated?.uiNumTicks,
      sliderScaling: existing?.sliderScaling ?? generated?.sliderScaling,
    });
  };
  const chain = collectRbxDomClassChain(classes, className);
  for (const chainClassName of chain) {
    const classInfo = classes[chainClassName];
    for (const [name, property] of Object.entries(classInfo?.Properties ?? {})) {
      if (hasGeneratedPropertyList(generatedMetadata, className) && !isGeneratedPropertyVisible(generatedMetadata, className, name)) {
        continue;
      }
      if (!isVisibleStudioPropertyForNode(pseudoNode, name, property, classes, chainClassName)) {
        continue;
      }
      const defaultRaw = classInfo?.DefaultProperties?.[name];
      setTemplate(name, property, defaultRaw === undefined ? "" : unwrapDefaultPropertyValue(defaultRaw, property, database), chainClassName);
    }
    for (const [name, defaultRaw] of Object.entries(classInfo?.DefaultProperties ?? {})) {
      if (hasGeneratedPropertyList(generatedMetadata, className) && !isGeneratedPropertyVisible(generatedMetadata, className, name)) {
        continue;
      }
      const property = findRbxDomProperty(classes, className, name);
      if (!isVisibleStudioPropertyForNode(pseudoNode, name, property, classes, chainClassName)) {
        continue;
      }
      setTemplate(name, property, unwrapDefaultPropertyValue(defaultRaw, property, database), chainClassName);
    }
  }
  const fallbackValueProperty = fallbackValueInstanceProperty(className, "Value");
  if (fallbackValueProperty && !rows.has("Value")) {
    setTemplate(
      "Value",
      fallbackValueProperty,
      defaultValueForDataType(propertyDataType(fallbackValueProperty), database),
      className,
    );
  }
  for (const [name, info] of Object.entries(generatedMetadata?.classes?.[className] ?? {})) {
    if (rows.has(name) || info.visible === false) {
      continue;
    }
    const dataType = info.type;
    const property = propertyFromGeneratedInfo(info);
    const enumItems = enumItemsForGeneratedInfo(info, property, database);
    rows.set(name, {
      name,
      displayName: info.displayName && info.displayName !== name ? info.displayName : undefined,
      defaultValue: defaultValueForDataType(dataType, database, enumItems),
      readonly: info.writable === false,
      category: info.category ?? propertyCategory(className, name, property, generatedMetadata),
      order: propertyOrder(generatedMetadata, className, name, nextOrder++),
      dataType,
      enumItems,
      uiMinimum: info.uiMinimum,
      uiMaximum: info.uiMaximum,
      uiNumTicks: info.uiNumTicks,
      sliderScaling: info.sliderScaling,
    });
  }
  const templates = sortPropertyRows(Array.from(rows.values()));
  propertyTemplateCache.set(className, templates);
  return templates;
}

function propertyRowsForNode(node: FileExplorerNode): PropertyRow[] {
  const config = getExplorerConfig();
  const database = loadRbxDomDatabase(config) ?? {};
  const generatedMetadata = loadGeneratedRobloxProperties(config);
  const classes = database.Classes;
  const rows = new Map<string, PropertyRow>();
  let nextOrder = 0;
  const setRow = (name: string, row: Omit<PropertyRow, "name">): void => {
    if (usesDisabledProperty(node.className) && name === "Disabled") {
      const existingEnabled = rows.get("Enabled");
      const disabledValue = row.value === true || String(row.value).toLowerCase() === "true";
      rows.set("Enabled", {
        name: "Enabled",
        displayName: "Enabled",
        value: !disabledValue,
        readonly: row.readonly,
        defaulted: row.defaulted,
        category: row.category,
        order: existingEnabled?.order ?? row.order,
        dataType: "Bool",
        enumItems: row.enumItems,
      });
      return;
    }
    if (usesDisabledProperty(node.className) && name === "Enabled") {
      const existingEnabled = rows.get("Enabled");
      if (existingEnabled && !existingEnabled.defaulted) {
        return;
      }
      if (existingEnabled && row.defaulted) {
        return;
      }
      rows.set("Enabled", {
        name: "Enabled",
        displayName: "Enabled",
        value: row.value === true || String(row.value).toLowerCase() === "true",
        readonly: row.readonly,
        defaulted: row.defaulted,
        category: row.category,
        order: existingEnabled?.order ?? row.order,
        dataType: "Bool",
        enumItems: row.enumItems,
      });
      return;
    }
    rows.set(name, { name, ...row });
  };
  const finalizeRows = (): PropertyRow[] => withStudioDuplicatePropertyRows(sortPropertyRows(Array.from(rows.values())), node);
  if (!classes) {
    const generatedValueInfo = generatedPropertyInfo(generatedMetadata, node.className, "Value");
    const generatedValueProperty = propertyFromGeneratedInfo(generatedValueInfo);
    const fallbackValueProperty = fallbackValueInstanceProperty(node.className, "Value");
    const valueProperty = generatedValueProperty ?? fallbackValueProperty;
    if (valueProperty) {
      rows.set("Value", {
        name: "Value",
        displayName: propertyDisplayName(generatedMetadata, node.className, "Value"),
        value: defaultValueForDataType(
          generatedValueInfo?.type ?? propertyDataType(valueProperty),
          database,
          enumItemsForGeneratedInfo(generatedValueInfo, valueProperty, database),
        ),
        readonly: false,
        defaulted: true,
        category: propertyCategory(node.className, "Value", valueProperty, generatedMetadata),
        order: nextOrder++,
        dataType: generatedValueInfo?.type ?? propertyDataType(valueProperty),
        uiMinimum: generatedValueInfo?.uiMinimum,
        uiMaximum: generatedValueInfo?.uiMaximum,
        uiNumTicks: generatedValueInfo?.uiNumTicks,
        sliderScaling: generatedValueInfo?.sliderScaling,
      });
    }
    for (const name of Object.keys(node.properties).filter((name) => !isMetadataPropertyName(name) && name !== "Tags" && name !== "Attributes")) {
      if (hasGeneratedPropertyList(generatedMetadata, node.className) && !isGeneratedPropertyVisible(generatedMetadata, node.className, name)) {
        continue;
      }
      const generated = generatedPropertyInfo(generatedMetadata, node.className, name);
      const property = propertyFromGeneratedInfo(generated) ?? fallbackValueInstanceProperty(node.className, name);
      const row: PropertyRow = {
        name,
        displayName: propertyDisplayName(generatedMetadata, node.className, name),
        value: node.properties[name],
        readonly: false,
        defaulted: false,
        category: propertyCategory(node.className, name, property, generatedMetadata),
        order: nextOrder++,
        dataType: generated?.type ?? propertyDataType(property),
        enumItems: generated?.enumItems,
        uiMinimum: generated?.uiMinimum,
        uiMaximum: generated?.uiMaximum,
        uiNumTicks: generated?.uiNumTicks,
        sliderScaling: generated?.sliderScaling,
      };
      if (usesDisabledProperty(node.className) && name === "Disabled") {
        const disabledValue = row.value === true || String(row.value).toLowerCase() === "true";
        rows.set("Enabled", { ...row, name: "Enabled", value: !disabledValue, dataType: "Bool" });
      } else {
        rows.set(name, row);
      }
    }
    ensureModelPivotRows(node, rows, classes, nextOrder);
    return finalizeRows();
  }
  const templates = propertyTemplatesForClass(node.className, database, classes, generatedMetadata);
  for (const template of templates) {
    setRow(template.name, {
      displayName: template.displayName,
      value: template.defaultValue,
      readonly: template.readonly,
      defaulted: true,
      category: template.category,
      order: template.order,
      dataType: template.dataType,
      enumItems: template.enumItems,
      uiMinimum: template.uiMinimum,
      uiMaximum: template.uiMaximum,
      uiNumTicks: template.uiNumTicks,
      sliderScaling: template.sliderScaling,
    });
  }
  nextOrder = templates.length;
  for (const propertyName of Object.keys(node.properties)) {
    if (hasGeneratedPropertyList(generatedMetadata, node.className) && !isGeneratedPropertyVisible(generatedMetadata, node.className, propertyName)) {
      continue;
    }
    const property = findRbxDomProperty(classes, node.className, propertyName);
    if (property && isSupersededMigratedProperty(node.className, property, classes)) {
      const targetName = propertyMigrationTarget(property);
      if (targetName && !Object.prototype.hasOwnProperty.call(node.properties, targetName)) {
        const targetRow = rows.get(targetName);
        if (targetRow && targetRow.defaulted) {
          rows.set(targetName, { ...targetRow, value: node.properties[propertyName], defaulted: false });
        }
      }
      continue;
    }
    const generated = generatedPropertyInfo(generatedMetadata, node.className, propertyName);
    const generatedProperty = propertyFromGeneratedInfo(generated);
    if (isMetadataPropertyName(propertyName) || propertyName === "Tags" || propertyName === "Attributes" || isEngineManagedStudioProperty(node.className, property, classes, propertyName)) {
      continue;
    }
    const existing = rows.get(propertyName);
    if (property && !isVisibleStudioPropertyForNode(node, propertyName, property, classes) && !existing) {
      continue;
    }
    setRow(propertyName, {
      displayName: existing?.displayName ?? propertyDisplayName(generatedMetadata, node.className, propertyName),
      value: node.properties[propertyName],
      readonly: existing?.readonly ?? isReadonlyStudioPropertyForNode(node, propertyName, property, classes),
      defaulted: false,
      category: existing?.category ?? propertyCategory(node.className, propertyName, property, generatedMetadata),
      order: existing?.order ?? propertyOrder(generatedMetadata, node.className, propertyName, nextOrder++),
      dataType: existing?.dataType ?? generated?.type ?? propertyDataType(property),
      enumItems: existing?.enumItems ?? enumItemsForGeneratedInfo(generated, property ?? generatedProperty, database),
      uiMinimum: existing?.uiMinimum ?? generated?.uiMinimum,
      uiMaximum: existing?.uiMaximum ?? generated?.uiMaximum,
      uiNumTicks: existing?.uiNumTicks ?? generated?.uiNumTicks,
      sliderScaling: existing?.sliderScaling ?? generated?.sliderScaling,
    });
  }
  ensureModelPivotRows(node, rows, classes, nextOrder);
  return finalizeRows();
}

function withStudioDuplicatePropertyRows(rows: PropertyRow[], node: FileExplorerNode): PropertyRow[] {
  if (node.className !== "Workspace") {
    return rows;
  }
  const streamingEnabled = rows.find((row) => row.name === "StreamingEnabled");
  if (!streamingEnabled || rows.some((row) => row.name === "StreamingEnabled" && row.category === "Streaming")) {
    return rows;
  }
  return sortPropertyRows([
    ...rows,
    {
      ...streamingEnabled,
      category: "Streaming",
      order: 2,
    },
  ]);
}

function defaultCFrameValue(): unknown {
  return { _type: "CFrame", components: [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1] };
}

function modelPivotValue(node: FileExplorerNode): unknown {
  return node.properties.WorldPivot ?? node.properties.WorldPivotData ?? node.properties.Origin ?? defaultCFrameValue();
}

function isModelPivotCFrameProperty(node: FileExplorerNode, name: string): boolean {
  return MODEL_PIVOT_CLASSES.has(node.className) && (name === "WorldPivot" || name === "WorldPivotData" || name === "Origin");
}

function ensureModelPivotRows(
  node: FileExplorerNode,
  rows: Map<string, PropertyRow>,
  classes: Record<string, RbxDomClass> | undefined,
  baseOrder: number,
): void {
  if (!MODEL_PIVOT_CLASSES.has(node.className)) {
    return;
  }
  rows.delete("WorldPivotData");
  rows.delete("Origin");
  const propertyFor = (name: string): RbxDomProperty | undefined => classes
    ? findRbxDomProperty(classes, node.className, name)
    : undefined;
  const put = (
    name: string,
    value: unknown,
    property: RbxDomProperty | undefined,
    dataType: string | undefined,
    orderOffset: number,
  ): void => {
    if (property && !isVisibleStudioPropertyForNode(node, name, property, classes)) {
      return;
    }
    const existing = rows.get(name);
    rows.set(name, {
      name,
      displayName: existing?.displayName,
      value: existing?.defaulted === false ? existing.value : value,
      readonly: existing?.readonly ?? isReadonlyStudioPropertyForNode(node, name, property, classes),
      defaulted: existing?.defaulted ?? !Object.prototype.hasOwnProperty.call(node.properties, name),
      category: existing?.category ?? "Pivot",
      order: existing?.order ?? baseOrder + orderOffset,
      dataType: existing?.dataType ?? dataType,
      enumItems: existing?.enumItems,
      uiMinimum: existing?.uiMinimum,
      uiMaximum: existing?.uiMaximum,
      uiNumTicks: existing?.uiNumTicks,
      sliderScaling: existing?.sliderScaling,
    });
  };

  const primaryPartProperty = propertyFor("PrimaryPart");
  put("PrimaryPart", node.properties.PrimaryPart ?? null, primaryPartProperty, propertyDataType(primaryPartProperty) ?? "Ref", 1);

  if (node.className !== "Workspace") {
    const scaleProperty = propertyFor("Scale");
    put("Scale", node.properties.Scale ?? 1, scaleProperty, propertyDataType(scaleProperty) ?? "Float32", 2);
  }

  const worldPivotProperty = propertyFor("WorldPivot");
  put("WorldPivot", modelPivotValue(node), worldPivotProperty, propertyDataType(worldPivotProperty) ?? "CFrame", 3);
}

function recordValue(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined;
}

function numberMember(record: Record<string, unknown> | undefined, name: string, fallback = 0): number {
  const raw = record?.[name];
  return typeof raw === "number" && Number.isFinite(raw) ? raw : fallback;
}

function pascalNumberMember(record: Record<string, unknown> | undefined, lowerName: string, upperName: string, fallback = 0): number {
  return numberMember(record, lowerName, numberMember(record, upperName, fallback));
}

function enumNameFromBytecodeValue(value: unknown, dataType?: string): string {
  const record = recordValue(value);
  if (record?._type === "EnumItem") {
    return String(record.name ?? record.Name ?? "");
  }
  if (typeof value === "string") {
    const enumType = dataType?.startsWith("Enum.") ? dataType : undefined;
    return enumType && value.startsWith(`${enumType}.`) ? value.slice(enumType.length + 1) : value.split(".").pop() ?? value;
  }
  return "";
}

function enumValuesForDataType(dataType: string | undefined, database: RbxDomDatabase, fallbackItems?: string[]): Array<{ name: string; value: number }> | undefined {
  if (!dataType?.startsWith("Enum.")) {
    return undefined;
  }
  const enumType = dataType.slice("Enum.".length);
  const items = database.Enums?.[enumType]?.items;
  if (items) {
    return Object.entries(items)
      .sort((a, b) => a[1] - b[1])
      .map(([name, value]) => ({ name, value }));
  }
  return fallbackItems?.map((name, index) => ({ name, value: index }));
}

function verdeTypeForValue(value: unknown, dataType?: string): string {
  if (dataType?.startsWith("Enum.")) {
    return dataType;
  }
  switch (dataType) {
    case "Bool":
      return "boolean";
    case "Int32":
    case "Int64":
      return "int";
    case "Float32":
    case "Float64":
    case "Double":
      return "number";
    case "String":
    case "BinaryString":
    case "ProtectedString":
      return "string";
    case "Content":
    case "ContentId":
      return "ContentId";
    case "Vector2":
    case "Vector3":
    case "UDim":
    case "UDim2":
    case "CFrame":
    case "OptionalCFrame":
    case "Color3":
    case "BrickColor":
    case "NumberRange":
    case "NumberSequence":
    case "ColorSequence":
      return dataType === "OptionalCFrame" ? "CFrame" : dataType;
    case "Ref":
    case "Instance":
      return "Ref";
    default:
      break;
  }
  const record = recordValue(value);
  const typeName = record?._type;
  if (typeof typeName === "string") {
    return typeName === "EnumItem" ? dataType ?? "string" : typeName;
  }
  if (typeof value === "boolean") {
    return "boolean";
  }
  if (typeof value === "number") {
    return Number.isInteger(value) ? "int" : "number";
  }
  return "string";
}

function color3ToVerde(value: unknown): { R: number; G: number; B: number } {
  const record = recordValue(value);
  return {
    R: pascalNumberMember(record, "r", "R"),
    G: pascalNumberMember(record, "g", "G"),
    B: pascalNumberMember(record, "b", "B"),
  };
}

function brickColorNumberFromValue(value: unknown, fallback = 194): number {
  const record = recordValue(value);
  const raw = record?.number ?? record?.Number ?? record?.BrickColor ?? value;
  const number = typeof raw === "number" ? raw : Number(raw);
  return Number.isFinite(number) ? Math.trunc(number) : fallback;
}

function brickColorToVerde(value: unknown): { Number: number } {
  return { Number: brickColorNumberFromValue(value) };
}

function refToVerde(value: unknown): unknown {
  if (value === null || value === undefined) {
    return null;
  }
  if (typeof value === "string") {
    const text = value.trim();
    if (text.length === 0 || /^none|null$/i.test(text)) {
      return null;
    }
    if (text.includes(".")) {
      const pathKey = refPathKeyFromText(text);
      return {
        _type: "Ref",
        pathSegments: pathKey ? pathKey.split("\0") : text.split(".").map((segment) => segment.trim()).filter((segment) => segment.length > 0),
      };
    }
    return {
      _type: "Ref",
      settingsId: text,
      instanceId: text,
    };
  }
  const record = recordValue(value);
  if (!record) {
    return value;
  }
  if (record._type === "Ref") {
    return record;
  }
  const legacyRef = recordValue(record.Ref);
  if (legacyRef) {
    return { _type: "Ref", ...legacyRef };
  }
  return { _type: "Ref", ...record };
}

function refRecordFromValue(value: unknown): Record<string, unknown> | undefined {
  if (typeof value === "string") {
    const text = value.trim();
    if (text.length === 0 || /^none|null$/i.test(text)) {
      return undefined;
    }
    if (text.includes(".")) {
      return {
        _type: "Ref",
        pathSegments: text.split(".").map((segment) => segment.trim()).filter((segment) => segment.length > 0),
      };
    }
    return {
      _type: "Ref",
      settingsId: text,
      instanceId: text,
    };
  }
  const record = recordValue(value);
  if (!record) {
    return undefined;
  }
  if (record._type === "Ref") {
    return record;
  }
  return recordValue(record.Ref);
}

function refPathSegmentsFromRecord(record: Record<string, unknown> | undefined): string[] {
  const segments = Array.isArray(record?.pathSegments) ? record.pathSegments : Array.isArray(record?.PathSegments) ? record.PathSegments : [];
  return segments.map((segment) => String(segment)).filter((segment) => segment.length > 0);
}

function refDisplayText(value: unknown, target?: FileExplorerNode): string {
  if (value === null || value === undefined) {
    return "None";
  }
  if (target) {
    return target.pathSegments.length > 0 ? target.pathSegments.join(".") : target.name;
  }
  const record = refRecordFromValue(value);
  if (!record) {
    return String(value);
  }
  const pathSegments = refPathSegmentsFromRecord(record);
  if (pathSegments.length > 0) {
    return pathSegments.join(".");
  }
  const named = record.name ?? record.Name;
  if (typeof named === "string" && named.length > 0) {
    return named;
  }
  const settingsId = record.settingsId ?? record.instanceId;
  if (typeof settingsId === "string" && settingsId.length > 0) {
    return settingsId;
  }
  const instanceIndex = record.instanceIndex;
  if (typeof instanceIndex === "number" && Number.isFinite(instanceIndex)) {
    return `Instance #${Math.trunc(instanceIndex)}`;
  }
  const referent = record.referent ?? record.ref;
  if (typeof referent === "string" && referent.length > 0) {
    return referent;
  }
  return "None";
}

function brickColorDisplayText(value: unknown): string {
  return String(brickColorNumberFromValue(value));
}

function vector2ToVerde(value: unknown): { X: number; Y: number } {
  const record = recordValue(value);
  return {
    X: pascalNumberMember(record, "x", "X"),
    Y: pascalNumberMember(record, "y", "Y"),
  };
}

function vector3ToVerde(value: unknown): { X: number; Y: number; Z: number } {
  const record = recordValue(value);
  return {
    X: pascalNumberMember(record, "x", "X"),
    Y: pascalNumberMember(record, "y", "Y"),
    Z: pascalNumberMember(record, "z", "Z"),
  };
}

function udimToVerde(value: unknown): { Scale: number; Offset: number } {
  const record = recordValue(value);
  return {
    Scale: pascalNumberMember(record, "scale", "Scale"),
    Offset: pascalNumberMember(record, "offset", "Offset"),
  };
}

function udim2ToVerde(value: unknown): { X: { Scale: number; Offset: number }; Y: { Scale: number; Offset: number } } {
  const record = recordValue(value);
  const x = recordValue(record?.X);
  const y = recordValue(record?.Y);
  return {
    X: {
      Scale: pascalNumberMember(record, "xScale", "XScale", numberMember(x, "Scale")),
      Offset: pascalNumberMember(record, "xOffset", "XOffset", numberMember(x, "Offset")),
    },
    Y: {
      Scale: pascalNumberMember(record, "yScale", "YScale", numberMember(y, "Scale")),
      Offset: pascalNumberMember(record, "yOffset", "YOffset", numberMember(y, "Offset")),
    },
  };
}

function cframeComponents(value: unknown): number[] {
  const record = recordValue(value);
  const components = Array.isArray(record?.components) ? record.components : Array.isArray(record?.Components) ? record.Components : [];
  if (components.length === 0 && record) {
    const position = Array.isArray(record.position) ? record.position : Array.isArray(record.Position) ? record.Position : undefined;
    const orientation = Array.isArray(record.orientation) ? record.orientation : Array.isArray(record.Orientation) ? record.Orientation : undefined;
    if (position || orientation) {
      const row0 = Array.isArray(orientation?.[0]) ? orientation[0] : [1, 0, 0];
      const row1 = Array.isArray(orientation?.[1]) ? orientation[1] : [0, 1, 0];
      const row2 = Array.isArray(orientation?.[2]) ? orientation[2] : [0, 0, 1];
      return [
        typeof position?.[0] === "number" ? position[0] : 0,
        typeof position?.[1] === "number" ? position[1] : 0,
        typeof position?.[2] === "number" ? position[2] : 0,
        typeof row0[0] === "number" ? row0[0] : 1,
        typeof row0[1] === "number" ? row0[1] : 0,
        typeof row0[2] === "number" ? row0[2] : 0,
        typeof row1[0] === "number" ? row1[0] : 0,
        typeof row1[1] === "number" ? row1[1] : 1,
        typeof row1[2] === "number" ? row1[2] : 0,
        typeof row2[0] === "number" ? row2[0] : 0,
        typeof row2[1] === "number" ? row2[1] : 0,
        typeof row2[2] === "number" ? row2[2] : 1,
      ];
    }
  }
  const out = components.map((item) => typeof item === "number" && Number.isFinite(item) ? item : 0);
  while (out.length < 12) {
    out.push(out.length === 3 || out.length === 7 || out.length === 11 ? 1 : 0);
  }
  return out.slice(0, 12);
}

function cframeToVerde(value: unknown): { Position: { X: number; Y: number; Z: number }; Rotation: { X: number; Y: number; Z: number } } {
  const components = cframeComponents(value);
  return {
    Position: { X: components[0] ?? 0, Y: components[1] ?? 0, Z: components[2] ?? 0 },
    Rotation: { X: 0, Y: 0, Z: 0 },
  };
}

function sequenceToVerde(value: unknown, valueKind: "number" | "color"): { Keypoints: unknown[] } {
  const record = recordValue(value);
  const keypoints = Array.isArray(record?.keypoints) ? record.keypoints : Array.isArray(record?.Keypoints) ? record.Keypoints : [];
  return {
    Keypoints: keypoints.map((raw) => {
      const keypoint = recordValue(raw);
      if (valueKind === "color") {
        return {
          Time: pascalNumberMember(keypoint, "time", "Time"),
          Value: color3ToVerde(keypoint?.value ?? keypoint?.color ?? keypoint?.Value),
        };
      }
      return {
        Time: pascalNumberMember(keypoint, "time", "Time"),
        Value: pascalNumberMember(keypoint, "value", "Value"),
        Envelope: pascalNumberMember(keypoint, "envelope", "Envelope"),
      };
    }),
  };
}

function contentToVerdeString(value: unknown): string {
  if (typeof value === "string") return value;
  const record = recordValue(value);
  if (record) {
    for (const key of ["Uri", "uri", "Url", "url"]) {
      const uri = record[key];
      if (typeof uri === "string") return uri;
    }
    return "";
  }
  return String(value);
}

function valueToVerde(value: unknown, type: string, database: RbxDomDatabase, enumItems?: string[]): unknown {
  if (type.startsWith("Enum.")) {
    const name = enumNameFromBytecodeValue(value, type);
    const enumValue = enumValuesForDataType(type, database, enumItems)?.find((item) => item.name === name)?.value ?? 0;
    return { Name: name, Value: enumValue, EnumType: type };
  }
  switch (type) {
    case "boolean":
      return value === true || String(value).toLowerCase() === "true";
    case "number":
    case "int":
    case "float":
    case "double":
      return typeof value === "number" ? value : Number(value) || 0;
    case "Color3":
      return color3ToVerde(value);
    case "BrickColor":
      return brickColorToVerde(value);
    case "Ref":
      return refToVerde(value);
    case "Vector2":
      return vector2ToVerde(value);
    case "Vector3":
      return vector3ToVerde(value);
    case "UDim":
      return udimToVerde(value);
    case "UDim2":
      return udim2ToVerde(value);
    case "CFrame":
    case "OptionalCFrame":
      return cframeToVerde(value);
    case "NumberRange": {
      const record = recordValue(value);
      return { Min: pascalNumberMember(record, "min", "Min"), Max: pascalNumberMember(record, "max", "Max") };
    }
    case "NumberSequence":
      return sequenceToVerde(value, "number");
    case "ColorSequence":
      return sequenceToVerde(value, "color");
    case "ContentId":
    case "string":
      return value === undefined || value === null ? "" : contentToVerdeString(value);
    default:
      return value === undefined || value === null ? "" : typeof value === "object" ? JSON.stringify(value) : value;
  }
}

function verdePropertyRowsForNode(node: FileExplorerNode, parentName: string, resolveReference?: (value: unknown) => FileExplorerNode | undefined): VerdePropertiesData {
  const database = loadRbxDomDatabase(getExplorerConfig()) ?? {};
  const metadataLocked = node.kind === "service" || isProtectedStarterPlayerContainer(node);
  const properties: VerdePropertyInfo[] = [
    { name: "Name", type: "string", value: node.name, category: "Data", layoutOrder: -3, isReadOnly: metadataLocked },
    { name: "ClassName", type: "string", value: node.className, category: "Data", layoutOrder: -2, isReadOnly: true },
    { name: "Parent", type: "string", value: parentName || "game", category: "Data", layoutOrder: -1, isReadOnly: true },
  ];
  for (const row of propertyRowsForNode(node)) {
    const type = verdeTypeForValue(row.value, row.dataType);
    const enumValues = enumValuesForDataType(row.dataType, database, row.enumItems);
    const value = valueToVerde(row.value, type, database, row.enumItems);
    const propertyInfo: VerdePropertyInfo = {
      name: row.name,
      displayName: row.displayName,
      type,
      value,
      category: row.category || "Data",
      layoutOrder: row.order,
      isEnum: type.startsWith("Enum."),
      enumValues,
      isReadOnly: row.readonly,
      uiMinimum: row.uiMinimum,
      uiMaximum: row.uiMaximum,
      uiNumTicks: row.uiNumTicks,
      sliderScaling: row.sliderScaling,
    };
    if (type === "BrickColor") {
      propertyInfo.displayValue = brickColorDisplayText(value);
    }
    if (type === "Ref") {
      const target = resolveReference?.(row.value) ?? resolveReference?.(value);
      const displayValue = refDisplayText(row.value, target);
      propertyInfo.isInstanceReference = true;
      propertyInfo.displayValue = displayValue;
      if (target) {
        propertyInfo.referencedInstanceId = target.treeId;
        propertyInfo.referencedInstanceName = displayValue;
        propertyInfo.referencedInstanceClass = target.className;
      } else if (displayValue !== "None") {
        propertyInfo.referencedInstanceName = displayValue;
      }
    }
    properties.push(propertyInfo);
  }
  return {
    properties,
    tags: searchTagsFromNode(node),
    attributes: Object.entries(node.attributes)
      .filter(([name]) => !name.startsWith("RBX_"))
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([name, value]) => {
        const type = verdeTypeForValue(value);
        return { name, type, value: valueToVerde(value, type, database) };
      }),
  };
}

function bytecodeColor3(value: Record<string, unknown> | undefined): unknown {
  return { _type: "Color3", r: numberMember(value, "R"), g: numberMember(value, "G"), b: numberMember(value, "B") };
}

function bytecodeBrickColor(value: unknown, currentValue: unknown): unknown {
  return { _type: "BrickColor", number: brickColorNumberFromValue(value, brickColorNumberFromValue(currentValue)) };
}

function bytecodeRef(value: unknown, currentValue: unknown): unknown {
  if (value === null || value === undefined) {
    return null;
  }
  if (typeof value === "string") {
    const text = value.trim();
    if (text.length === 0 || /^none|null$/i.test(text)) {
      return null;
    }
    if (text.includes(".")) {
      return {
        _type: "Ref",
        pathSegments: text.split(".").map((segment) => segment.trim()).filter((segment) => segment.length > 0),
      };
    }
    return {
      _type: "Ref",
      settingsId: text,
      instanceId: text,
    };
  }
  const record = recordValue(value);
  if (!record) {
    return currentValue;
  }
  if (record._type === "Ref") {
    return record;
  }
  const legacyRef = recordValue(record.Ref);
  if (legacyRef) {
    return { _type: "Ref", ...legacyRef };
  }
  return { _type: "Ref", ...record };
}

function bytecodeVector2(value: Record<string, unknown> | undefined): unknown {
  return { _type: "Vector2", x: numberMember(value, "X"), y: numberMember(value, "Y") };
}

function bytecodeVector3(value: Record<string, unknown> | undefined): unknown {
  return { _type: "Vector3", x: numberMember(value, "X"), y: numberMember(value, "Y"), z: numberMember(value, "Z") };
}

function bytecodeUdim(value: Record<string, unknown> | undefined): unknown {
  return { _type: "UDim", scale: numberMember(value, "Scale"), offset: numberMember(value, "Offset") };
}

function bytecodeUdim2(value: Record<string, unknown> | undefined): unknown {
  const x = recordValue(value?.X);
  const y = recordValue(value?.Y);
  return {
    _type: "UDim2",
    xScale: numberMember(x, "Scale"),
    xOffset: numberMember(x, "Offset"),
    yScale: numberMember(y, "Scale"),
    yOffset: numberMember(y, "Offset"),
  };
}

function bytecodeCFrame(value: Record<string, unknown> | undefined, currentValue: unknown): unknown {
  const components = cframeComponents(currentValue);
  const position = recordValue(value?.Position);
  if (position) {
    components[0] = numberMember(position, "X");
    components[1] = numberMember(position, "Y");
    components[2] = numberMember(position, "Z");
  }
  return { _type: "CFrame", components };
}

function bytecodeSequence(value: Record<string, unknown> | undefined, valueKind: "number" | "color"): unknown {
  const keypoints = Array.isArray(value?.Keypoints) ? value.Keypoints : [];
  return {
    _type: valueKind === "color" ? "ColorSequence" : "NumberSequence",
    keypoints: keypoints.map((raw) => {
      const keypoint = recordValue(raw);
      if (valueKind === "color") {
        return {
          time: pascalNumberMember(keypoint, "time", "Time"),
          value: bytecodeColor3(recordValue(keypoint?.Value)),
        };
      }
      return {
        time: pascalNumberMember(keypoint, "time", "Time"),
        value: pascalNumberMember(keypoint, "value", "Value"),
        envelope: pascalNumberMember(keypoint, "envelope", "Envelope"),
      };
    }),
  };
}

function bytecodeValueFromVerde(value: unknown, type: string | undefined, currentValue: unknown): unknown {
  if (type?.startsWith("Enum.")) {
    const record = recordValue(value);
    return {
      _type: "EnumItem",
      enumType: type,
      name: String(record?.EnumName ?? record?.Name ?? value ?? ""),
    };
  }
  const record = recordValue(value);
  switch (type) {
    case "BrickColor":
      return bytecodeBrickColor(value, currentValue);
    case "Ref":
      return bytecodeRef(value, currentValue);
    case "Color3":
      return bytecodeColor3(record);
    case "Vector2":
      return bytecodeVector2(record);
    case "Vector3":
      return bytecodeVector3(record);
    case "UDim":
      return bytecodeUdim(record);
    case "UDim2":
      return bytecodeUdim2(record);
    case "CFrame":
    case "OptionalCFrame":
      return bytecodeCFrame(record, currentValue);
    case "NumberRange":
      return { _type: "NumberRange", min: numberMember(record, "Min"), max: numberMember(record, "Max") };
    case "NumberSequence":
      return bytecodeSequence(record, "number");
    case "ColorSequence":
      return bytecodeSequence(record, "color");
    default:
      return value;
  }
}

function defaultAttributeValue(type: string): unknown {
  switch (type) {
    case "number":
      return 0;
    case "boolean":
      return false;
    case "Color3":
      return { _type: "Color3", r: 0, g: 0, b: 0 };
    case "Vector2":
      return { _type: "Vector2", x: 0, y: 0 };
    case "Vector3":
      return { _type: "Vector3", x: 0, y: 0, z: 0 };
    case "UDim":
      return { _type: "UDim", scale: 0, offset: 0 };
    case "UDim2":
      return { _type: "UDim2", xScale: 0, xOffset: 0, yScale: 0, yOffset: 0 };
    case "NumberRange":
      return { _type: "NumberRange", min: 0, max: 0 };
    case "NumberSequence":
      return { _type: "NumberSequence", keypoints: [{ time: 0, value: 0, envelope: 0 }, { time: 1, value: 0, envelope: 0 }] };
    case "ColorSequence":
      return { _type: "ColorSequence", keypoints: [{ time: 0, value: { _type: "Color3", r: 0, g: 0, b: 0 } }, { time: 1, value: { _type: "Color3", r: 1, g: 1, b: 1 } }] };
    default:
      return "";
  }
}

function searchValueText(value: unknown): string {
  if (value === null || value === undefined) {
    return "";
  }
  if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function searchValueTexts(record: Record<string, unknown>): string[] {
  return Object.entries(record)
    .filter(([name]) => name !== "Source" && name !== "LinkedSource")
    .map(([, value]) => searchValueText(value))
    .filter((value) => value.length > 0);
}

function searchRecordPairs(record: Record<string, unknown>): Array<[string, string]> {
  const pairs: Array<[string, string]> = [];
  for (const [name, value] of Object.entries(record)) {
    if (name === "Source" || name === "LinkedSource") {
      continue;
    }
    pairs.push([name, searchValueText(value)]);
  }
  return pairs;
}

function searchSafeRecord(record: Record<string, unknown>): Record<string, unknown> {
  if (!Object.prototype.hasOwnProperty.call(record, "Source") && !Object.prototype.hasOwnProperty.call(record, "LinkedSource")) {
    return record;
  }
  const filtered: Record<string, unknown> = {};
  for (const [name, value] of Object.entries(record)) {
    if (name === "Source" || name === "LinkedSource") {
      continue;
    }
    filtered[name] = value;
  }
  return filtered;
}

function cloneBytecodeRecord(record: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [name, value] of Object.entries(record)) {
    if (name === "Source" || name === "LinkedSource") {
      continue;
    }
    const encoded = JSON.stringify(value);
    if (encoded !== undefined) {
      out[name] = JSON.parse(encoded) as unknown;
    }
  }
  return out;
}

function clonePathKey(pathSegments: unknown): string | undefined {
  if (!Array.isArray(pathSegments)) {
    return undefined;
  }
  const parts = pathSegments.map((segment) => String(segment));
  return parts.length > 0 ? parts.join("\0") : undefined;
}

function refPathKeyFromSegments(pathSegments: unknown): string | undefined {
  if (!Array.isArray(pathSegments)) {
    return undefined;
  }
  const parts = pathSegments.map((segment) => String(segment).trim()).filter((segment) => segment.length > 0);
  const normalized = parts[0]?.toLowerCase() === "game" ? parts.slice(1) : parts;
  return clonePathKey(normalized);
}

function refPathKeyFromText(value: string): string | undefined {
  const text = value.trim();
  if (text.length === 0 || /^none|null$/i.test(text)) {
    return undefined;
  }
  return refPathKeyFromSegments(text.split("."));
}

function emptyCloneTargetMap(): CloneTargetMap {
  return {
    bySettingsId: new Map(),
    byIndex: new Map(),
    byPathKey: new Map(),
  };
}

function collectCloneIdentities(snapshot: CloneSnapshot): CloneIdentitySet {
  const identities: CloneIdentitySet = {
    settingsIds: new Set(),
    indices: new Set(),
    pathKeys: new Set(),
  };
  const visit = (current: CloneSnapshot) => {
    if (current.settingsId) {
      identities.settingsIds.add(current.settingsId);
    }
    if (typeof current.index === "number") {
      identities.indices.add(current.index);
    }
    const pathKey = clonePathKey(current.pathSegments);
    if (pathKey) {
      identities.pathKeys.add(pathKey);
    }
    for (const child of current.children) {
      visit(child);
    }
  };
  visit(snapshot);
  return identities;
}

function rememberCloneTarget(snapshot: CloneSnapshot, target: FileExplorerNode, cloneTargets: CloneTargetMap): void {
  if (snapshot.settingsId) {
    cloneTargets.bySettingsId.set(snapshot.settingsId, target);
  }
  if (typeof snapshot.index === "number") {
    cloneTargets.byIndex.set(snapshot.index, target);
  }
  const pathKey = clonePathKey(snapshot.pathSegments);
  if (pathKey) {
    cloneTargets.byPathKey.set(pathKey, target);
  }
}

function findCloneTarget(snapshot: CloneSnapshot, cloneTargets: CloneTargetMap): FileExplorerNode | undefined {
  if (snapshot.settingsId) {
    const bySettingsId = cloneTargets.bySettingsId.get(snapshot.settingsId);
    if (bySettingsId) {
      return bySettingsId;
    }
  }
  if (typeof snapshot.index === "number") {
    const byIndex = cloneTargets.byIndex.get(snapshot.index);
    if (byIndex) {
      return byIndex;
    }
  }
  const pathKey = clonePathKey(snapshot.pathSegments);
  return pathKey ? cloneTargets.byPathKey.get(pathKey) : undefined;
}

function refTargetFromObject(object: Record<string, unknown>): { settingsId?: string; index?: number; pathKey?: string } {
  const instanceIndex = typeof object.instanceIndex === "number" ? object.instanceIndex : undefined;
  const zeroIndex = instanceIndex !== undefined && Number.isFinite(instanceIndex) ? Math.trunc(instanceIndex) - 1 : undefined;
  const settingsId = typeof object.settingsId === "string"
    ? object.settingsId
    : typeof object.instanceId === "string"
      ? object.instanceId
      : undefined;
  return {
    settingsId,
    index: zeroIndex !== undefined && zeroIndex >= 0 ? zeroIndex : undefined,
    pathKey: refPathKeyFromSegments(object.pathSegments ?? object.PathSegments),
  };
}

function isInternalCloneRef(value: unknown, identities: CloneIdentitySet): boolean {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  const object = value as Record<string, unknown>;
  if (object._type === "Ref") {
    const target = refTargetFromObject(object);
    return (
      (target.settingsId !== undefined && identities.settingsIds.has(target.settingsId)) ||
      (target.index !== undefined && identities.indices.has(target.index)) ||
      (target.pathKey !== undefined && identities.pathKeys.has(target.pathKey))
    );
  }
  const legacyRef = object.Ref;
  if (legacyRef && typeof legacyRef === "object" && !Array.isArray(legacyRef)) {
    return isInternalCloneRef({ _type: "Ref", ...(legacyRef as Record<string, unknown>) }, identities);
  }
  return Object.values(object).some((nested) => containsInternalCloneRef(nested, identities));
}

function containsInternalCloneRef(value: unknown, identities: CloneIdentitySet): boolean {
  if (Array.isArray(value)) {
    return value.some((item) => containsInternalCloneRef(item, identities));
  }
  return isInternalCloneRef(value, identities);
}

function cloneRecordWithoutInternalRefs(record: Record<string, unknown>, identities: CloneIdentitySet): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [name, value] of Object.entries(record)) {
    if (containsInternalCloneRef(value, identities)) {
      continue;
    }
    const encoded = JSON.stringify(value);
    if (encoded !== undefined) {
      out[name] = JSON.parse(encoded) as unknown;
    }
  }
  return out;
}

function cloneTargetForRef(object: Record<string, unknown>, cloneTargets: CloneTargetMap): FileExplorerNode | undefined {
  const target = refTargetFromObject(object);
  if (target.settingsId !== undefined) {
    const bySettingsId = cloneTargets.bySettingsId.get(target.settingsId);
    if (bySettingsId) {
      return bySettingsId;
    }
  }
  if (target.index !== undefined) {
    const byIndex = cloneTargets.byIndex.get(target.index);
    if (byIndex) {
      return byIndex;
    }
  }
  return target.pathKey !== undefined ? cloneTargets.byPathKey.get(target.pathKey) : undefined;
}

function refValueForCloneTarget(target: FileExplorerNode): Record<string, unknown> {
  if (typeof target.index === "number") {
    return {
      _type: "Ref",
      instanceIndex: target.index + 1,
    };
  }
  return {
    _type: "Ref",
    pathSegments: target.pathSegments.slice(),
  };
}

function remapCloneRefs(value: unknown, cloneTargets: CloneTargetMap): { value: unknown; changed: boolean } {
  if (Array.isArray(value)) {
    let changed = false;
    const next = value.map((item) => {
      const remapped = remapCloneRefs(item, cloneTargets);
      changed = changed || remapped.changed;
      return remapped.value;
    });
    return { value: next, changed };
  }
  if (!value || typeof value !== "object") {
    return { value, changed: false };
  }

  const object = value as Record<string, unknown>;
  if (object._type === "Ref") {
    const target = cloneTargetForRef(object, cloneTargets);
    if (target) {
      return { value: refValueForCloneTarget(target), changed: true };
    }
  }
  const legacyRef = object.Ref;
  if (legacyRef && typeof legacyRef === "object" && !Array.isArray(legacyRef)) {
    const target = cloneTargetForRef(legacyRef as Record<string, unknown>, cloneTargets);
    if (target) {
      return { value: refValueForCloneTarget(target), changed: true };
    }
  }

  let changed = false;
  const out: Record<string, unknown> = {};
  for (const [key, nested] of Object.entries(object)) {
    const remapped = remapCloneRefs(nested, cloneTargets);
    changed = changed || remapped.changed;
    out[key] = remapped.value;
  }
  return { value: out, changed };
}

function appendRecordAssignments(args: string[], flag: string, record: Record<string, unknown>): void {
  for (const [name, value] of Object.entries(record)) {
    const encoded = JSON.stringify(value);
    if (encoded !== undefined) {
      args.push(flag, `${name}=${encoded}`);
    }
  }
}

function searchTagsFromNode(node: FileExplorerNode): string[] {
  const raw = node.properties.Tags ?? node.attributes.Tags ?? node.properties.tags ?? node.attributes.tags;
  if (Array.isArray(raw)) {
    return raw.map((value) => searchValueText(value)).filter((value) => value.length > 0);
  }
  const text = searchValueText(raw);
  return text ? [text] : [];
}

function runCli(command: string, args: string[], cwd: string): Promise<CommandRunResult> {
  return new Promise((resolve, reject) => {
    const child = childProcess.spawn(command, args, {
      cwd,
      env: process.env,
      shell: false,
      stdio: "pipe",
      windowsHide: true,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (data: Buffer | string) => {
      stdout += data.toString();
    });
    child.stderr.on("data", (data: Buffer | string) => {
      stderr += data.toString();
    });
    child.on("error", reject);
    child.on("close", (code) => {
      resolve({ code: code ?? 0, stdout, stderr });
    });
  });
}

async function runJsonCli<T>(config: ExplorerConfig, args: string[]): Promise<T> {
  if (!fs.existsSync(config.rustCliPath)) {
    throw new Error(`${RUST_CLI_BINARY} was not found at ${config.rustCliPath}. Build the CLI or point the "renium.rustCliPath" setting at it.`);
  }
  const result = await runCli(config.rustCliPath, args, config.projectRoot);
  if (result.code !== 0) {
    throw new Error((result.stderr || result.stdout || `Renium reported an error (code ${result.code}).`).trim());
  }
  return JSON.parse(result.stdout.trim()) as T;
}

async function runBytecodeBatchOne<T extends object>(
  config: ExplorerConfig,
  settingsFile: string,
  service: string,
  op: CliBatchOp,
): Promise<T & { service: string; settingsFile: string }> {
  const dump = await runJsonCli<CliBatchDump<T>>(config, [
    "bytecode-explorer-batch",
    "-f",
    settingsFile,
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

export class FileExplorerModel {
  private readonly sorter = new InstanceSorter();
  private readonly nodesById = new Map<string, FileExplorerNode>();
  private readonly searchLoadedServices = new Set<string>();
  private readonly searchOnlyNodeIds = new Set<string>();
  private rootIds: string[] = [];
  private readonly onChangeCallbacks: Array<() => void> = [];
  private studioMutationChain: Promise<void> = Promise.resolve();

  public onChange(callback: () => void): void {
    this.onChangeCallbacks.push(callback);
  }

  public getNode(treeId: string): FileExplorerNode | undefined {
    return this.nodesById.get(treeId);
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

  public rememberNode(node: FileExplorerNode): FileExplorerNode {
    const existing = this.nodesById.get(node.treeId);
    if (existing) {
      existing.id = node.id;
      existing.kind = node.kind;
      existing.service = node.service;
      existing.name = node.name;
      existing.className = node.className;
      existing.settingsId = node.settingsId;
      existing.index = node.index;
      existing.parentTreeId = node.parentTreeId;
      existing.hasChildren = node.hasChildren;
      existing.hasPackageLink = node.hasPackageLink;
      existing.settingsFile = node.settingsFile;
      existing.sourcePath = node.sourcePath ?? existing.sourcePath;
      existing.pathSegments = node.pathSegments.length > 0 ? node.pathSegments : existing.pathSegments;
      if (Object.keys(node.properties).length > 0) {
        existing.properties = node.properties;
      }
      if (Object.keys(node.attributes).length > 0) {
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

  public getChildren(node: FileExplorerNode): FileExplorerNode[] {
    return node.children
      .map((id) => this.nodesById.get(id))
      .filter((child): child is FileExplorerNode => child !== undefined);
  }

  public sort(nodes: FileExplorerNode[]): FileExplorerNode[] {
    return this.sorter.sortNodes(nodes) as unknown as FileExplorerNode[];
  }

  public async refresh(): Promise<void> {
    const config = getExplorerConfig();
    const root = srcRoot(config);
    const serviceNames = new Set<string>();
    this.searchLoadedServices.clear();
    if (fs.existsSync(root)) {
      for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
        if (entry.isDirectory()) {
          serviceNames.add(entry.name);
        }
      }
    }
    for (const service of config.services) {
      serviceNames.add(service);
    }

    const serviceList = Array.from(serviceNames).filter((service) => service.length > 0);

    this.nodesById.clear();
    this.searchOnlyNodeIds.clear();
    this.rootIds = serviceList
      .map((service) => {
        const settingsFile = settingsFileForService(config, service);
        const hasSettingsFile = fs.existsSync(settingsFile);
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
          hasChildren: hasSettingsFile,
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
    this.fireChange();
  }

  public async refreshServices(services: string[]): Promise<void> {
    const config = getExplorerConfig();
    const uniqueServices = Array.from(new Set(services.map((service) => String(service).trim()).filter((service) => service.length > 0)));
    let changed = false;
    for (const service of uniqueServices) {
      changed = await this.refreshService(config, service) || changed;
    }
    if (changed) {
      this.fireChange();
    }
  }

  public servicesFromSettingsFiles(settingsFiles: string[]): string[] {
    const config = getExplorerConfig();
    return Array.from(new Set(settingsFiles
      .map((settingsFile) => serviceFromSettingsFile(config, settingsFile))
      .filter((service): service is string => service !== undefined)));
  }

  private async refreshService(config: ExplorerConfig, service: string): Promise<boolean> {
    const serviceNode = this.nodesById.get(serviceTreeId(service));
    const settingsFile = settingsFileForService(config, service);
    if (!serviceNode || !fs.existsSync(settingsFile)) {
      await this.refresh();
      return false;
    }

    if (this.searchLoadedServices.has(service)) {
      await this.loadServiceDump(config, service, false);
      return true;
    }

    const loadedNodeIds = Array.from(this.nodesById.values())
      .filter((node) => node.service === service && node.loaded)
      .sort((a, b) => a.pathSegments.length - b.pathSegments.length)
      .map((node) => node.treeId);

    if (loadedNodeIds.length === 0) {
      try {
        const counts = await runBytecodeBatchOne<CliExplorerCounts>(config, settingsFile, service, { type: "counts" });
        serviceNode.hasChildren = (counts.rootChildren ?? 0) > 0;
        serviceNode.settingsFile = settingsFile;
        return true;
      } catch {
        return false;
      }
    }

    for (const treeId of loadedNodeIds) {
      const node = this.nodesById.get(treeId);
      if (node) {
        await this.loadChildren(node, false);
      }
    }
    return true;
  }

  private async readRootChildCounts(config: ExplorerConfig, services: string[]): Promise<Map<string, number>> {
    const entries = await Promise.all(services.map(async (service): Promise<[string, number]> => {
      const settingsFile = settingsFileForService(config, service);
      if (!fs.existsSync(settingsFile)) {
        return [service, 0];
      }
      try {
        const counts = await runBytecodeBatchOne<CliExplorerCounts>(config, settingsFile, service, { type: "counts" });
        return [service, counts.rootChildren ?? 0];
      } catch {
        return [service, 1];
      }
    }));
    return new Map(entries);
  }

  public async ensureLoaded(node: FileExplorerNode): Promise<FileExplorerNode> {
    const current = this.nodesById.get(node.treeId) ?? node;
    if (current.loaded) {
      return current;
    }
    await this.loadChildren(current);
    return this.nodesById.get(current.treeId) ?? current;
  }

  public async loadDetails(node: FileExplorerNode): Promise<FileExplorerNode> {
    const loaded = this.nodesById.get(node.treeId) ?? node;
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
      id: loaded.settingsId,
    });
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

  public async loadSearchCorpus(onProgress?: (progress: SearchLoadProgress) => void): Promise<void> {
    const config = getExplorerConfig();
    const roots = this.getRoots();
    const pending = roots.filter((root) => !this.searchLoadedServices.has(root.service));
    let loaded = roots.length - pending.length;
    if (loaded > 0) {
      onProgress?.({ loaded, total: roots.length, service: "cached" });
    }
    let cursor = 0;
    const workerCount = Math.min(4, pending.length);
    const workers = Array.from({ length: workerCount }, async () => {
      for (;;) {
        const root = pending[cursor];
        cursor += 1;
        if (!root) {
          return;
        }
        await this.loadServiceDump(config, root.service, false);
        loaded += 1;
        onProgress?.({ loaded, total: roots.length, service: root.service });
      }
    });
    await Promise.all(workers);
    this.fireChange();
  }

  public async searchDescendants(query: string, onProgress?: (progress: SearchLoadProgress) => void): Promise<void> {
    const config = getExplorerConfig();
    const roots = this.getRoots();
    this.clearSearchOnlyNodes();
    if (!query.trim()) {
      this.fireChange();
      return;
    }
    let loaded = 0;
    let cursor = 0;
    const workerCount = Math.min(4, roots.length);
    const workers = Array.from({ length: workerCount }, async () => {
      for (;;) {
        const root = roots[cursor];
        cursor += 1;
        if (!root) {
          return;
        }
        await this.loadSearchDump(config, root.service, query);
        loaded += 1;
        onProgress?.({ loaded, total: roots.length, service: root.service });
      }
    });
    await Promise.all(workers);
    this.fireChange();
  }

  public clearSearchResults(): void {
    this.clearSearchOnlyNodes();
    this.fireChange();
  }

  private clearSearchOnlyNodes(): void {
    if (this.searchOnlyNodeIds.size === 0) {
      return;
    }
    for (const node of this.nodesById.values()) {
      node.searchMatched = false;
      if (node.children.length > 0) {
        node.children = node.children.filter((childId) => !this.searchOnlyNodeIds.has(childId));
      }
    }
    for (const treeId of this.searchOnlyNodeIds) {
      this.nodesById.delete(treeId);
    }
    this.searchOnlyNodeIds.clear();
  }

  private async loadSearchDump(config: ExplorerConfig, service: string, query: string): Promise<void> {
    const settingsFile = settingsFileForService(config, service);
    const serviceNode = this.nodesById.get(serviceTreeId(service));
    if (!serviceNode || !fs.existsSync(settingsFile)) {
      return;
    }
    const dump = await runBytecodeBatchOne<CliSearchDump>(config, settingsFile, service, {
      type: "search",
      q: query,
    });
    const matchedTreeIds = new Set((dump.matchIds ?? []).map((settingsId) => normalizeId(service, settingsId)));
    const rootRaw = dump.nodes.find((raw) => !raw.parentId) ?? dump.nodes.find((raw) => raw.settingsId === dump.rootIds[0]);
    if (rootRaw) {
      serviceNode.settingsId = rootRaw.settingsId;
      serviceNode.index = rootRaw.index;
      serviceNode.name = rootRaw.name;
      serviceNode.className = rootRaw.className;
      serviceNode.pathSegments = rootRaw.pathSegments ?? [service];
      serviceNode.pathOrdinals = rootRaw.pathOrdinals ?? [1];
      serviceNode.sourcePath = rootRaw.sourcePath;
      serviceNode.properties = safeObject(rootRaw.properties);
      serviceNode.attributes = safeObject(rootRaw.attributes);
      serviceNode.hasChildren = (rootRaw.childCount ?? rootRaw.children?.length ?? serviceNode.children.length) > 0;
      serviceNode.hasPackageLink = rootRaw.hasPackageLink === true;
      serviceNode.searchMatched = matchedTreeIds.has(serviceNode.treeId);
      this.mergeChildren(serviceNode, (rootRaw.children ?? []).map((childId) => normalizeId(service, childId)));
    }

    for (const raw of dump.nodes) {
      if (rootRaw && raw.settingsId === rootRaw.settingsId) {
        continue;
      }
      const treeId = normalizeId(service, raw.settingsId);
      const existed = this.nodesById.has(treeId);
      const childNode: FileExplorerNode = {
        id: treeId,
        treeId,
        kind: "instance",
        service,
        settingsId: raw.settingsId,
        index: raw.index,
        name: raw.name,
        className: raw.className,
        parentTreeId: raw.parentId === rootRaw?.settingsId
          ? serviceNode.treeId
          : raw.parentId
            ? normalizeId(service, raw.parentId)
            : serviceNode.treeId,
        children: (raw.children ?? []).map((childId) => normalizeId(service, childId)),
        loaded: false,
        detailsLoaded: false,
        hasChildren: (raw.childCount ?? raw.children?.length ?? 0) > 0,
        hasPackageLink: raw.hasPackageLink === true,
        settingsFile: dump.settingsFile,
        sourcePath: raw.sourcePath,
        pathSegments: raw.pathSegments ?? [],
        pathOrdinals: raw.pathOrdinals ?? [],
        properties: safeObject(raw.properties),
        attributes: safeObject(raw.attributes),
        searchMatched: matchedTreeIds.has(treeId),
      };
      const existing = this.nodesById.get(treeId);
      if (existing && !this.searchOnlyNodeIds.has(treeId)) {
        existing.settingsId = childNode.settingsId;
        existing.index = childNode.index;
        existing.name = childNode.name;
        existing.className = childNode.className;
        existing.parentTreeId = childNode.parentTreeId;
        this.mergeChildren(existing, childNode.children);
        existing.detailsLoaded = existing.detailsLoaded && Object.keys(existing.properties).length > 0;
        existing.hasChildren = childNode.hasChildren;
        existing.hasPackageLink = childNode.hasPackageLink;
        existing.sourcePath = childNode.sourcePath;
        existing.pathSegments = childNode.pathSegments;
        existing.pathOrdinals = childNode.pathOrdinals;
        if (Object.keys(childNode.properties).length > 0) {
          existing.properties = childNode.properties;
        }
        if (Object.keys(childNode.attributes).length > 0) {
          existing.attributes = childNode.attributes;
        }
        existing.searchMatched = childNode.searchMatched;
      } else {
        this.nodesById.set(treeId, childNode);
      }
      if (!existed) {
        this.searchOnlyNodeIds.add(treeId);
      }
      const parent = childNode.parentTreeId ? this.nodesById.get(childNode.parentTreeId) : undefined;
      if (parent) {
        this.mergeChildren(parent, [treeId]);
      }
    }
  }

  private mergeChildren(node: FileExplorerNode, childIds: string[]): void {
    if (childIds.length === 0) {
      return;
    }
    const seen = new Set(node.children);
    for (const childId of childIds) {
      if (!seen.has(childId)) {
        node.children.push(childId);
        seen.add(childId);
      }
    }
  }

  private async loadServiceDump(config: ExplorerConfig, service: string, notify = true): Promise<void> {
    const settingsFile = settingsFileForService(config, service);
    const serviceNode = this.nodesById.get(serviceTreeId(service));
    if (!serviceNode || !fs.existsSync(settingsFile)) {
      this.searchLoadedServices.add(service);
      return;
    }
    const dump = await runBytecodeBatchOne<CliServiceDump>(config, settingsFile, service, { type: "service" });
    const rootId = dump.rootIds[0];
    const rootRaw = dump.nodes.find((raw) => raw.settingsId === rootId) ?? dump.nodes.find((raw) => !raw.parentId);
    if (!rootRaw) {
      serviceNode.children = [];
      serviceNode.loaded = true;
      serviceNode.detailsLoaded = true;
      serviceNode.hasChildren = false;
      this.searchLoadedServices.add(service);
      return;
    }

    this.removeKnownChildren(serviceNode);
    serviceNode.settingsId = rootRaw.settingsId;
    serviceNode.index = rootRaw.index;
    serviceNode.name = rootRaw.name;
    serviceNode.className = rootRaw.className;
    serviceNode.pathSegments = rootRaw.pathSegments ?? [service];
    serviceNode.pathOrdinals = rootRaw.pathOrdinals ?? [1];
    serviceNode.sourcePath = rootRaw.sourcePath;
    serviceNode.properties = safeObject(rootRaw.properties);
    serviceNode.attributes = safeObject(rootRaw.attributes);
    serviceNode.children = (rootRaw.children ?? []).map((childId) => normalizeId(service, childId));
    serviceNode.hasChildren = (rootRaw.childCount ?? serviceNode.children.length) > 0;
    serviceNode.hasPackageLink = rootRaw.hasPackageLink === true;
    serviceNode.loaded = true;
    serviceNode.detailsLoaded = true;

    for (const raw of dump.nodes) {
      if (raw.settingsId === rootRaw.settingsId) {
        continue;
      }
      const treeId = normalizeId(service, raw.settingsId);
      const childNode: FileExplorerNode = {
        id: treeId,
        treeId,
        kind: "instance",
        service,
        settingsId: raw.settingsId,
        index: raw.index,
        name: raw.name,
        className: raw.className,
        parentTreeId: raw.parentId === rootRaw.settingsId
          ? serviceNode.treeId
          : raw.parentId
            ? normalizeId(service, raw.parentId)
            : serviceNode.treeId,
        children: (raw.children ?? []).map((childId) => normalizeId(service, childId)),
        loaded: true,
        detailsLoaded: true,
        hasChildren: (raw.childCount ?? raw.children?.length ?? 0) > 0,
        hasPackageLink: raw.hasPackageLink === true,
        settingsFile: dump.settingsFile,
        sourcePath: raw.sourcePath,
        pathSegments: raw.pathSegments ?? [],
        pathOrdinals: raw.pathOrdinals ?? [],
        properties: safeObject(raw.properties),
        attributes: safeObject(raw.attributes),
      };
      this.nodesById.set(treeId, childNode);
    }
    this.searchLoadedServices.add(service);
    if (notify) {
      this.fireChange();
    }
  }

  public async loadChildren(node: FileExplorerNode, notify = true): Promise<void> {
    const config = getExplorerConfig();
    const settingsFile = settingsFileForService(config, node.service);
    if (!fs.existsSync(settingsFile)) {
      node.loaded = true;
      node.detailsLoaded = true;
      node.hasChildren = false;
      node.children = [];
      if (notify) {
        this.fireChange();
      }
      return;
    }

    const op: CliBatchOp = { type: "children" };
    if (node.kind !== "service") {
      if (node.settingsId) {
        op.id = node.settingsId;
      } else if (node.index !== undefined) {
        op.x = node.index;
      } else {
        return;
      }
    }

    const dump = await runBytecodeBatchOne<CliChildrenDump>(config, settingsFile, node.service, op);
    const parentRaw = dump.parent;
    node.settingsId = parentRaw.settingsId;
    node.index = parentRaw.index;
    node.name = parentRaw.name;
    node.className = parentRaw.className;
    node.pathSegments = parentRaw.pathSegments ?? node.pathSegments;
    node.pathOrdinals = parentRaw.pathOrdinals ?? node.pathOrdinals;
    node.properties = safeObject(parentRaw.properties);
    node.attributes = safeObject(parentRaw.attributes);
    node.sourcePath = parentRaw.sourcePath;
    node.hasChildren = (parentRaw.childCount ?? dump.children.length) > 0;
    node.hasPackageLink = parentRaw.hasPackageLink === true;
    node.loaded = true;
    node.detailsLoaded = true;

    this.removeKnownChildren(node);
    node.children = dump.children.map((raw) => normalizeId(node.service, raw.settingsId));
    for (const raw of dump.children) {
      const treeId = normalizeId(node.service, raw.settingsId);
      const childNode: FileExplorerNode = {
        id: treeId,
        treeId,
        kind: "instance",
        service: dump.service,
        settingsId: raw.settingsId,
        index: raw.index,
        name: raw.name,
        className: raw.className,
        parentTreeId: raw.parentId === parentRaw.settingsId
          ? node.treeId
          : raw.parentId
            ? normalizeId(dump.service, raw.parentId)
            : serviceTreeId(dump.service),
        children: [],
        loaded: false,
        detailsLoaded: true,
        hasChildren: (raw.childCount ?? raw.children?.length ?? 0) > 0,
        hasPackageLink: raw.hasPackageLink === true,
        settingsFile: dump.settingsFile,
        sourcePath: raw.sourcePath,
        pathSegments: raw.pathSegments ?? [],
        pathOrdinals: raw.pathOrdinals ?? [],
        properties: safeObject(raw.properties),
        attributes: safeObject(raw.attributes),
      };
      this.nodesById.set(treeId, childNode);
    }
    if (notify) {
      this.fireChange();
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

  public async setValue(node: FileExplorerNode, scope: "metadata" | "property" | "attribute", name: string, valueText: string, options: { skipStudioPush?: boolean } = {}): Promise<void> {
    const loaded = await this.ensureDetails(node);
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
    if (scope === "property" && usesDisabledProperty(loaded.className) && name === "Enabled") {
      propertyName = "Disabled";
      parsedValue = !(parsedValue === true);
    }
    if (options.skipStudioPush) {
      await vscode.commands.executeCommand("renium.noteProgrammaticEditorWrite", {
        paths: [loaded.settingsFile],
        durationMs: 5000,
      });
    }
    const args = [
      "bytecode-set-property",
      "-f",
      loaded.settingsFile,
      "-p",
      propertyName,
      `--value-json=${JSON.stringify(parsedValue)}`,
      "-S",
      scope,
    ];
    if (loaded.settingsId) {
      args.push("-i", loaded.settingsId);
    } else {
      args.push("-x", String(loaded.index));
    }
    await runJsonCli(config, args);
    if (options.skipStudioPush) {
      await vscode.commands.executeCommand("renium.noteProgrammaticEditorWrite", {
        paths: [loaded.settingsFile],
        durationMs: 5000,
        refreshCache: true,
      });
    }
    if (scope === "metadata") {
      await this.reloadParentChildren(loaded);
    } else {
      if (scope === "property") {
        loaded.properties[propertyName] = parsedValue;
      } else {
        loaded.attributes[propertyName] = parsedValue;
      }
      loaded.detailsLoaded = true;
      this.fireChange();
    }
    if (!options.skipStudioPush) {
      await this.pushSettingsToStudio(loaded.settingsFile);
    }
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
    for (const candidate of this.nodesById.values()) {
      if (candidate.service !== node.service || candidate.treeId === node.treeId || !candidate.settingsId) {
        continue;
      }
      if (
        candidate.settingsId === text ||
        candidate.treeId === text ||
        candidate.name === text ||
        candidate.pathSegments.join(".") === text
      ) {
        return candidate.settingsId;
      }
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
      const config = getExplorerConfig();
      const args = [
        "bytecode-move-instance",
        "-f",
        loaded.settingsFile,
        "-s",
        loaded.service,
        "-i",
        loaded.settingsId,
        "--target-file",
        loadedParent.settingsFile,
        "--target-service",
        loadedParent.service,
      ];
      if (loadedParent.settingsId) {
        args.push("-I", loadedParent.settingsId);
      } else if (loadedParent.index !== undefined) {
        args.push("-X", String(loadedParent.index));
      }
      const result = await runJsonCli<CliMoveInstanceResult>(config, args);
      const copiedSourcePaths: string[] = [];
      for (const copy of result.sourceCopies ?? []) {
        if (copy.to) {
          copiedSourcePaths.push(copy.to);
        }
      }
      await this.loadService(loaded.service);
      await this.loadService(loadedParent.service);
      await this.pushSettingsToStudio(loadedParent.settingsFile, result.settingsIds, copiedSourcePaths);
      await this.pushSettingsToStudio(loaded.settingsFile);
      return result.rootSettingsId ? this.nodesById.get(normalizeId(loadedParent.service, result.rootSettingsId)) : undefined;
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
    const loadedParent = await this.ensureLoaded(parent);
    if (loadedParent.index === undefined) {
      throw new Error("Parent instance has no bytecode index.");
    }
    const config = getExplorerConfig();
    const args = [
      "bytecode-add-instance",
      "-f",
      loadedParent.settingsFile,
      "-x",
      String(loadedParent.index),
      "-c",
      className,
      "-n",
      name,
    ];
    appendRecordAssignments(args, "-p", properties);
    appendRecordAssignments(args, "-a", attributes);
    const result = await runJsonCli<{ settingsId?: string }>(config, args);
    await this.loadChildren(loadedParent);
    if (pushToStudio) {
      await this.pushSettingsToStudio(
        loadedParent.settingsFile,
        result.settingsId ? [result.settingsId] : undefined,
      );
    }
    return result.settingsId ? this.nodesById.get(normalizeId(loadedParent.service, result.settingsId)) : undefined;
  }

  public async cloneInstance(source: FileExplorerNode, parent: FileExplorerNode): Promise<FileExplorerNode | undefined> {
    if (source.kind === "service") {
      throw new Error("Service roots cannot be copied.");
    }
    const loadedSource = await this.ensureDetails(await this.ensureLoaded(source));
    const loadedParent = await this.ensureLoaded(parent);
    if (loadedSource.service !== loadedParent.service) {
      throw new Error("Cross-service copies need a subtree copy path before they can be full fidelity.");
    }
    if (!loadedSource.settingsId) {
      throw new Error("Selected instance has no bytecode id.");
    }
    if (!loadedParent.settingsId && loadedParent.index === undefined) {
      throw new Error("Target parent has no bytecode id.");
    }
    const config = getExplorerConfig();
    const args = [
      "bytecode-clone-instance",
      "-f",
      loadedParent.settingsFile,
      "-s",
      loadedParent.service,
      "-i",
      loadedSource.settingsId,
    ];
    if (loadedParent.settingsId) {
      args.push("-I", loadedParent.settingsId);
    } else if (loadedParent.index !== undefined) {
      args.push("-X", String(loadedParent.index));
    }
    const result = await runJsonCli<CliCloneInstanceResult>(config, args);
    const copiedSourcePaths: string[] = [];
    for (const copy of result.sourceCopies ?? []) {
      if (!copy.from || !copy.to || !fs.existsSync(copy.from)) {
        continue;
      }
      fs.mkdirSync(path.dirname(copy.to), { recursive: true });
      fs.copyFileSync(copy.from, copy.to);
      copiedSourcePaths.push(copy.to);
    }
    await this.loadChildren(loadedParent);
    const current = result.rootSettingsId
      ? this.nodesById.get(normalizeId(loadedParent.service, result.rootSettingsId))
      : undefined;
    await this.pushSettingsToStudio(loadedParent.settingsFile, result.settingsIds, copiedSourcePaths);
    return current;
  }

  public async exportModel(node: FileExplorerNode, outputPath: string, format: RobloxModelFormat): Promise<CliExportModelResult> {
    const loaded = await this.ensureLoaded(node);
    if (!loaded.settingsId && loaded.index === undefined) {
      throw new Error("Selected instance has no bytecode id.");
    }
    const config = getExplorerConfig();
    const args = [
      "bytecode-export-model",
      "-f",
      loaded.settingsFile,
      "-s",
      loaded.service,
      "-o",
      outputPath,
      "--format",
      format,
    ];
    if (loaded.settingsId) {
      args.push("-i", loaded.settingsId);
    } else if (loaded.index !== undefined) {
      args.push("-x", String(loaded.index));
    }
    return runJsonCli<CliExportModelResult>(config, args);
  }

  public async importModel(parent: FileExplorerNode, modelPath: string): Promise<FileExplorerNode | undefined> {
    const loadedParent = await this.ensureLoaded(parent);
    if (!loadedParent.settingsId && loadedParent.index === undefined) {
      throw new Error("Target parent has no bytecode id.");
    }
    const config = getExplorerConfig();
    const args = [
      "bytecode-import-model",
      "-f",
      loadedParent.settingsFile,
      "-s",
      loadedParent.service,
      "-m",
      modelPath,
    ];
    if (loadedParent.settingsId) {
      args.push("-I", loadedParent.settingsId);
    } else if (loadedParent.index !== undefined) {
      args.push("-x", String(loadedParent.index));
    }
    const result = await runJsonCli<CliImportModelResult>(config, args);
    await this.loadChildren(loadedParent);
    const created = result.rootSettingsIds?.[0]
      ? this.nodesById.get(normalizeId(loadedParent.service, result.rootSettingsIds[0]))
      : undefined;
    const sourceWritePaths = (result.sourceWrites ?? [])
      .map((write) => write.path)
      .filter((writePath): writePath is string => typeof writePath === "string" && writePath.length > 0);
    await this.pushSettingsToStudio(loadedParent.settingsFile, result.settingsIds, sourceWritePaths);
    return created;
  }

  private async snapshotSubtree(node: FileExplorerNode): Promise<CloneSnapshot> {
    const loaded = await this.ensureDetails(await this.ensureLoaded(node));
    const children = await Promise.all(this.sort(this.getChildren(loaded)).map((child) => this.snapshotSubtree(child)));
    return {
      settingsId: loaded.settingsId,
      index: loaded.index,
      pathSegments: loaded.pathSegments.slice(),
      name: loaded.name,
      className: loaded.className,
      properties: cloneBytecodeRecord(loaded.properties),
      attributes: cloneBytecodeRecord(loaded.attributes),
      sourcePath: loaded.sourcePath,
      children,
    };
  }

  private async insertSnapshot(
    snapshot: CloneSnapshot,
    parent: FileExplorerNode,
    cloneIdentities: CloneIdentitySet,
    cloneTargets: CloneTargetMap,
  ): Promise<FileExplorerNode | undefined> {
    const loadedParent = await this.ensureLoaded(parent);
    const name = this.uniqueChildName(loadedParent, snapshot.name);
    const created = await this.addInstance(
      loadedParent,
      snapshot.className,
      name,
      cloneRecordWithoutInternalRefs(snapshot.properties, cloneIdentities),
      snapshot.attributes,
      false,
    );
    if (!created) {
      return undefined;
    }
    const loadedCreated = await this.ensureDetails(await this.ensureLoaded(created));
    rememberCloneTarget(snapshot, loadedCreated, cloneTargets);
    for (const child of snapshot.children) {
      await this.insertSnapshot(child, loadedCreated, cloneIdentities, cloneTargets);
    }
    await this.loadChildren(loadedCreated);
    const current = this.nodesById.get(loadedCreated.treeId) ?? loadedCreated;
    await this.copySnapshotSource(snapshot, current);
    return current;
  }

  private async applyCloneRefRemaps(snapshot: CloneSnapshot, cloneTargets: CloneTargetMap): Promise<void> {
    const target = findCloneTarget(snapshot, cloneTargets);
    if (target) {
      for (const [name, value] of Object.entries(snapshot.properties)) {
        const remapped = remapCloneRefs(value, cloneTargets);
        if (remapped.changed) {
          await this.setValue(target, "property", name, JSON.stringify(remapped.value), { skipStudioPush: true });
        }
      }
    }
    for (const child of snapshot.children) {
      await this.applyCloneRefRemaps(child, cloneTargets);
    }
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

  private async copySnapshotSource(snapshot: CloneSnapshot, target: FileExplorerNode): Promise<void> {
    if (!snapshot.sourcePath || !fs.existsSync(snapshot.sourcePath) || !isScriptClass(snapshot.className)) {
      return;
    }
    const loadedTarget = await this.ensureDetails(target);
    if (!loadedTarget.sourcePath) {
      return;
    }
    fs.mkdirSync(path.dirname(loadedTarget.sourcePath), { recursive: true });
    fs.copyFileSync(snapshot.sourcePath, loadedTarget.sourcePath);
  }

  public async removeInstance(node: FileExplorerNode): Promise<CliRemoveInstanceResult> {
    const loaded = await this.ensureLoaded(node);
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
    let result: CliRemoveInstanceResult | undefined;
    for (let attempt = 0; ; attempt++) {
      try {
        result = await runJsonCli<CliRemoveInstanceResult>(config, [
          "bytecode-remove-instance",
          "-f",
          loaded.settingsFile,
          "-i",
          loaded.settingsId,
        ]);
        break;
      } catch (error) {
        if (attempt >= 2 || !isSettingsLockTimeout(error)) {
          throw error;
        }
        await delay(150 * (attempt + 1));
      }
    }
    this.removeSubtree(loaded.treeId);
    if (parent) {
      parent.children = parent.children.filter((childId) => childId !== loaded.treeId);
      parent.hasChildren = parent.children.length > 0;
    }
    this.fireChange();
    this.queueStudioMutation(async () => {
      try {
        await this.pushDeleteToStudio(studioDeleteTarget);
      } catch {
        await this.pushSettingsToStudio(loaded.settingsFile);
      }
    }, "delete instance");
    return result ?? {};
  }

  public async desyncPackageLink(node: FileExplorerNode): Promise<CliDesyncPackageLinkResult> {
    const loaded = await this.ensureLoaded(node);
    if (loaded.kind === "service") {
      throw new Error("Select a package root or PackageLink instance.");
    }
    if (!loaded.settingsId) {
      throw new Error("Selected instance has no bytecode id.");
    }
    const config = getExplorerConfig();
    const result = await runJsonCli<CliDesyncPackageLinkResult>(config, [
      "bytecode-desync-package-link",
      "-f",
      loaded.settingsFile,
      "-i",
      loaded.settingsId,
    ]);
    const removedLinks = Array.isArray(result.removedPackageLinks) ? result.removedPackageLinks : [];
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
    this.fireChange();
    try {
      for (const target of studioDeleteTargets) {
        await this.pushDeleteToStudio(target);
      }
    } catch {
      await this.pushSettingsToStudio(loaded.settingsFile);
    }
    if (loaded.className === "PackageLink") {
      if (parent) {
        await this.loadChildren(parent, false);
      } else {
        await this.loadService(loaded.service);
      }
    } else {
      await this.loadChildren(loaded, false);
    }
    this.fireChange();
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

  private collectSubtreeSettingsIds(node: FileExplorerNode): string[] {
    const out: string[] = [];
    const visit = (current: FileExplorerNode) => {
      if (current.settingsId) {
        out.push(current.settingsId);
      }
      for (const child of this.getChildren(current)) {
        visit(child);
      }
    };
    visit(node);
    return Array.from(new Set(out));
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
      skipChangeFilter: true,
      taskName: "Explorer -> Studio sync",
      targetSettingsIds,
    });
  }

  public async pushPropertiesToStudio(settingsFile: string, node: FileExplorerNode, propertyNames: string[]): Promise<void> {
    const targetProperties = Array.from(new Set(propertyNames
      .map((name) => String(name).trim())
      .filter((name) => name.length > 0)));
    if (!node.settingsId || targetProperties.length === 0) {
      await this.pushSettingsToStudio(settingsFile);
      return;
    }
    await vscode.commands.executeCommand("renium.pushEditorPathsNow", [settingsFile], {
      skipChangeFilter: true,
      taskName: "Explorer -> Studio sync",
      targetSettingsId: node.settingsId,
      targetProperties,
    });
  }

  public async pushPropertyToStudio(
    node: FileExplorerNode,
    scope: "metadata" | "property" | "attribute",
    propertyName: string,
    value: unknown,
  ): Promise<void> {
    const targetProperty = String(propertyName).trim();
    if (targetProperty.length === 0) {
      return;
    }
    const pathSegments = node.pathSegments.length > 0
      ? node.pathSegments.slice()
      : [node.service, node.name].filter((segment) => segment.length > 0);
    await vscode.commands.executeCommand("renium.pushEditorPropertyNow", {
      settingsFile: node.settingsFile,
      service: node.service,
      settingsId: node.settingsId,
      className: node.className,
      pathSegments,
      pathOrdinals: node.pathOrdinals.slice(),
      scope,
      property: targetProperty,
      value,
      allowProtectedMeshIdApply: node.className === "MeshPart" && targetProperty === "MeshId",
    });
  }

  public async pushDeleteToStudio(node: FileExplorerNode): Promise<void> {
    const pathSegments = node.pathSegments.length > 0
      ? node.pathSegments.slice()
      : [node.service, node.name].filter((segment) => segment.length > 0);
    await vscode.commands.executeCommand("renium.pushEditorDeleteNow", {
      settingsFile: node.settingsFile,
      service: node.service,
      settingsId: node.settingsId,
      className: node.className,
      pathSegments,
      pathOrdinals: node.pathOrdinals.slice(),
    });
  }

  private queueStudioMutation(task: () => Promise<void>, label: string): void {
    this.studioMutationChain = this.studioMutationChain
      .catch(() => undefined)
      .then(task)
      .catch((error) => {
        vscode.window.showErrorMessage(
          `Renium: failed to ${label} in Studio. ${error instanceof Error ? error.message : String(error)}`,
        );
      });
  }

  private fireChange(): void {
    for (const callback of this.onChangeCallbacks) {
      callback();
    }
  }
}

class FilePropertiesViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = "renium.properties";
  private webviewView: vscode.WebviewView | undefined;
  private currentNode: FileExplorerNode | undefined;
  private currentPackageMessage: PropertiesUpdateMessage | undefined;
  private webviewReady = false;
  private readonly pendingPropertyFinalSets = new Map<string, { node: FileExplorerNode; name: string; value: unknown }>();
  private readonly pendingPropertyHistory = new Map<string, PropertyEditHistoryItem>();
  private readonly propertyUndoStack: PropertyEditHistoryItem[] = [];
  private readonly propertyRedoStack: PropertyEditHistoryItem[] = [];
  private propertyHistorySequence = 0;
  private propertyFinalSetActive = false;
  private readonly pendingLiveStudioPushes = new Map<string, {
    node: FileExplorerNode;
    scope: "metadata" | "property";
    propertyName: string;
    value: unknown;
  }>();
  private liveStudioPushTimer: NodeJS.Timeout | undefined;
  private studioPropertyPushChain: Promise<void> = Promise.resolve();
  private referenceRevealHandler: ((nodeId: string) => Promise<void> | void) | undefined;

  public constructor(
    private readonly model: FileExplorerModel,
    private readonly extensionUri: vscode.Uri,
    private readonly onVisibilityChanged?: ViewVisibilityHandler,
  ) {}

  public setReferenceRevealHandler(handler: (nodeId: string) => Promise<void> | void): void {
    this.referenceRevealHandler = handler;
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
      void this.onMessage(message);
    });
    this.onVisibilityChanged?.(FilePropertiesViewProvider.viewType, webviewView.visible);
    webviewView.onDidChangeVisibility(() => {
      this.onVisibilityChanged?.(FilePropertiesViewProvider.viewType, webviewView.visible);
    });
    webviewView.webview.html = getPropertiesHtml(this.extensionUri, { showFilterInput: true });
  }

  public async show(node: FileExplorerNode): Promise<void> {
    this.currentPackageMessage = undefined;
    this.currentNode = await this.model.loadDetails(node);
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
    const data = verdePropertyRowsForNode(node, packageName);
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

  /** Show a decoded .renium instance's inline properties in the Properties view
   * (read-only) — used by the Inspector tab, which has no live backend node. */
  public showReadonlyInstance(info: {
    name?: string;
    className?: string;
    settingsId?: string;
    properties?: Record<string, unknown>;
    attributes?: Record<string, unknown>;
    pathSegments?: string[];
  }): void {
    const nodeName = cleanPropertyText(info.name) || "Instance";
    const className = cleanPropertyText(info.className) || "Instance";
    const pathSegments = sanitizePathSegments(info.pathSegments);
    const displayPath = pathSegments.length > 0 ? pathSegments.join(".") : nodeName;
    const node: FileExplorerNode = {
      id: `rbsync:${info.settingsId ?? displayPath}`,
      treeId: `rbsync:${info.settingsId ?? displayPath}`,
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
    const data = verdePropertyRowsForNode(node, "");
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
    const updated = (this.currentNode.settingsId
      ? this.model.getNode(normalizeId(this.currentNode.service, this.currentNode.settingsId))
      : this.model.getNode(serviceTreeId(this.currentNode.service))) ?? this.currentNode;
    updated.detailsLoaded = false;
    this.currentNode = await this.model.loadDetails(updated);
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
        (value) => this.currentNode ? this.model.findNodeForReference(value, this.currentNode.service) : undefined,
      ),
      nodeName: this.currentNode.name,
      nodeClassName: this.currentNode.className,
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
          vscode.window.showErrorMessage(`Renium: failed to reveal referenced instance. ${error instanceof Error ? error.message : String(error)}`);
        }
      }
      return;
    }
    if (!this.currentNode || !message.type) {
      return;
    }
    try {
      let shouldPush = true;
      switch (message.type) {
        case "setProperty":
          await this.queuePropertyFromWebview(message.propertyName, message.propertyValue, message.live === true);
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
      vscode.window.showErrorMessage(`Renium: failed to update property. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async queuePropertyFromWebview(name: string | undefined, value: unknown, live: boolean): Promise<void> {
    const node = this.currentNode;
    if (!node || !name) {
      return;
    }
    if (live) {
      await this.setPropertyFromWebview(node, name, value, true);
      return;
    }
    this.pendingPropertyFinalSets.set(`${node.treeId}:${name}`, { node, name, value });
    if (this.propertyFinalSetActive) {
      return;
    }
    this.propertyFinalSetActive = true;
    try {
      while (this.pendingPropertyFinalSets.size > 0) {
        const next = this.pendingPropertyFinalSets.values().next().value;
        if (!next) {
          break;
        }
        this.pendingPropertyFinalSets.delete(`${next.node.treeId}:${next.name}`);
        await this.setPropertyFromWebview(next.node, next.name, next.value, false);
      }
    } finally {
      this.propertyFinalSetActive = false;
    }
  }

  private async setPropertyFromWebview(node: FileExplorerNode, name: string, value: unknown, live = false): Promise<void> {
    const scope = isMetadataPropertyName(name) ? "metadata" : "property";
    if (scope === "metadata" && name !== "Name") {
      return;
    }
    const row = scope === "property"
      ? propertyRowsForNode(node).find((candidate) => candidate.name === name)
      : undefined;
    const currentValue = scope === "property"
      ? isModelPivotCFrameProperty(node, name)
        ? modelPivotValue(node)
        : name === "Enabled" && usesDisabledProperty(node.className)
        ? !(node.properties.Disabled === true)
        : node.properties[name]
      : node.name;
    const writePropertyName = scope === "property" && (name === "WorldPivotData" || name === "Origin") && MODEL_PIVOT_CLASSES.has(node.className)
      ? "WorldPivot"
      : name;
    const studioPropertyName = scope === "property"
      ? name === "Enabled" && usesDisabledProperty(node.className)
        ? "Disabled"
        : name === "Origin" && MODEL_PIVOT_CLASSES.has(node.className)
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
    const settingsBackup = this.capturePropertyHistoryBackup(historyItem);
    this.applyLocalPropertyValue(node, scope, name, rawValue);
    await this.model.setValue(node, scope, writePropertyName, JSON.stringify(rawValue), { skipStudioPush: true });
    this.rememberPropertyHistory(historyItem, settingsBackup);
    this.pushCurrent();
    this.queueFinalStudioPropertyPush(pushTarget, scope, studioPropertyName, studioRawValue);
  }

  private capturePropertyHistoryBackup(item: PropertyEditHistoryItem): Buffer | undefined {
    const settingsFile = path.normalize(item.settingsFile);
    if (!settingsFile || !fs.existsSync(settingsFile)) {
      return undefined;
    }
    try {
      return fs.readFileSync(settingsFile);
    } catch {
      return undefined;
    }
  }

  private rememberPropertyHistory(item: PropertyEditHistoryItem, settingsBackup: Buffer | undefined): void {
    const historyId = this.writePropertyHistory(item, settingsBackup);
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

  private writePropertyHistory(item: PropertyEditHistoryItem, settingsBackup: Buffer | undefined): string | undefined {
    if (!settingsBackup || settingsBackup.length === 0) {
      return undefined;
    }
    const config = getExplorerConfig();
    const historyRoot = editorHistoryRoot(config);
    const createdUnixMs = Date.now();
    const sequence = ++this.propertyHistorySequence;
    const safeService = safeHistoryComponent(item.service);
    const safeProperty = safeHistoryComponent(item.propertyLabel ?? item.name);
    const id = `${createdUnixMs}-${sequence}-${safeService}-${safeProperty}`;
    const entryDir = path.join(historyRoot, id);
    const manifestPath = path.join(entryDir, "manifest.json");
    if (!pathInsideRoot(historyRoot, manifestPath)) {
      return undefined;
    }
    try {
      fs.mkdirSync(entryDir, { recursive: true });
      fs.writeFileSync(path.join(entryDir, "settings.renium"), settingsBackup);
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
        settingsBackup: "settings.renium",
      };
      fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
      return id;
    } catch {
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

  private async applyPropertyHistoryValue(item: PropertyEditHistoryItem, rawValue: unknown): Promise<void> {
    const currentMatches = !!this.currentNode && (
      this.currentNode.treeId === item.treeId
      || (!!item.settingsId && this.currentNode.settingsId === item.settingsId)
    );
    const node = this.model.getNode(item.treeId)
      ?? (item.settingsId ? this.model.getNode(normalizeId(item.service, item.settingsId)) : undefined)
      ?? (currentMatches ? this.currentNode : undefined);
    if (!node || node.service !== item.service) {
      return;
    }
    const pushTarget = this.snapshotStudioPushNode(node);
    const clonedValue = cloneHistoryValue(rawValue);
    this.applyLocalPropertyValue(node, item.scope, item.name, clonedValue);
    await this.model.setValue(node, item.scope, item.writePropertyName, JSON.stringify(clonedValue), { skipStudioPush: true });
    if (this.currentNode && (this.currentNode.treeId === node.treeId || (item.settingsId && this.currentNode.settingsId === item.settingsId))) {
      this.applyLocalPropertyValue(this.currentNode, item.scope, item.name, clonedValue);
      this.pushCurrent();
    }
    this.queueFinalStudioPropertyPush(pushTarget, item.scope, item.studioPropertyName, this.studioRawValueForHistory(item, clonedValue));
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
      } else if ((name === "WorldPivot" || name === "WorldPivotData" || name === "Origin") && MODEL_PIVOT_CLASSES.has(node.className)) {
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
    if (this.liveStudioPushTimer) {
      clearTimeout(this.liveStudioPushTimer);
      this.liveStudioPushTimer = undefined;
    }
    const pushes = Array.from(this.pendingLiveStudioPushes.values());
    this.pendingLiveStudioPushes.clear();
    await Promise.all(pushes.map((push) =>
      this.model.pushPropertyToStudio(push.node, push.scope, push.propertyName, push.value),
    ));
  }

  private queueFinalStudioPropertyPush(
    node: FileExplorerNode,
    scope: "metadata" | "property",
    propertyName: string,
    value: unknown,
  ): void {
    this.studioPropertyPushChain = this.studioPropertyPushChain
      .catch(() => undefined)
      .then(async () => {
        await this.flushLiveStudioPushes();
        await this.model.pushPropertyToStudio(node, scope, propertyName, value);
        await this.reloadCurrent();
      })
      .catch((error) => {
        vscode.window.showErrorMessage(
          `Renium: failed to update property in Studio. ${error instanceof Error ? error.message : String(error)}`,
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
    const updated = this.currentNode.settingsId
      ? this.model.getNode(normalizeId(this.currentNode.service, this.currentNode.settingsId))
      : this.model.getNode(serviceTreeId(this.currentNode.service));
    if (updated) {
      this.currentNode = await this.model.loadDetails(updated);
    }
  }

  private html(): string {
    return `<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src *; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
*{box-sizing:border-box}
:root{--property-editor-focus-border:rgba(128,128,128,.45)}
body{margin:0;padding:10px;font-family:var(--vscode-font-family);font-size:var(--vscode-font-size);color:var(--vscode-foreground);background:var(--vscode-sideBar-background)}
#empty{color:var(--vscode-descriptionForeground)}
.head{display:flex;align-items:center;gap:8px;margin-bottom:10px}
.title{font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.class{color:var(--vscode-descriptionForeground)}
.group{margin:10px 0 4px;font-size:11px;text-transform:uppercase;color:var(--vscode-descriptionForeground)}
.group.sub{margin-top:8px;text-transform:none;font-weight:600;color:var(--vscode-foreground)}
.row{display:grid;grid-template-columns:minmax(90px, 38%) minmax(0, 1fr);gap:8px;align-items:center;min-height:26px;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border, transparent)}
.row.readonly{opacity:.55}
.key{display:flex;align-items:center;height:100%;line-height:22px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--vscode-foreground)}
.editor{display:flex;align-items:center;min-width:0;height:100%;line-height:22px}
input.value{width:100%;min-width:0;height:22px;padding:2px 5px;border:1px solid var(--vscode-input-border, transparent);background:var(--vscode-input-background);color:var(--vscode-input-foreground);font:inherit;line-height:18px}
input.value:focus,input.value:focus-visible,input.color:focus,input.color:focus-visible,select.value:focus,select.value:focus-visible{border-color:var(--property-editor-focus-border)!important;background:var(--vscode-input-background)!important;outline:none!important;box-shadow:none!important}
input.bool{width:16px;height:16px;margin:0}
input.color{width:28px;min-width:28px;height:22px;padding:0;border:1px solid var(--vscode-input-border, transparent);background:var(--vscode-input-background)}
.colorWrap{display:flex;align-items:center;gap:6px;width:100%;min-width:0}
.colorText{flex:1}
input.range{flex:1;min-width:70px}
.rangeWrap{display:flex;align-items:center;gap:0;width:100%;min-width:0}
.rangeWrap:focus-within{gap:6px}
.rangeWrap:not(:focus-within) input.range{display:none}
.rangeNumber{flex:1;min-width:0;width:100%}
.rangeWrap:focus-within .rangeNumber{width:58px;min-width:58px;flex:0 0 58px}
select.value{width:100%;min-width:0;height:22px;border:1px solid var(--vscode-dropdown-border, transparent);background:var(--vscode-dropdown-background);color:var(--vscode-dropdown-foreground);font:inherit;line-height:20px}
input:disabled,select:disabled{cursor:default;background:var(--vscode-input-background);color:var(--vscode-disabledForeground);border-color:var(--vscode-input-border, transparent)}
.readonly input.bool{filter:grayscale(1);opacity:.75}
</style>
</head>
<body>
<div id="app"><div id="empty">Select an instance.</div></div>
<script>
(function(){
var vscode=acquireVsCodeApi();
var app=document.getElementById('app');
function esc(s){return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;')}
function enumTypeFromDataType(dataType){return dataType&&dataType.indexOf('Enum.')===0?dataType:null}
function enumNameFromValue(v,dataType){
  if(v&&typeof v==='object'&&!Array.isArray(v)&&v._type==='EnumItem')return String(v.name||'');
  if(typeof v==='string'){
    var enumType=enumTypeFromDataType(dataType);
    if(enumType&&v.indexOf(enumType+'.')===0)return v.slice(enumType.length+1);
    return v.split('.').pop();
  }
  return '';
}
function formatNumber(n){
  if(typeof n!=='number'||!isFinite(n))return String(n);
  var rounded=Math.round(n*1000)/1000;
  return String(Object.is(rounded,-0)?0:rounded);
}
function normalizeNumberInputText(text){
  var n=Number(text);
  return isFinite(n)?formatNumber(n):String(text);
}
function clampByte(n){return Math.max(0,Math.min(255,Math.round(n)))}
function isColor3(v){return !!v&&typeof v==='object'&&!Array.isArray(v)&&v._type==='Color3'}
function colorObj(v){
  if(isColor3(v))return {r:Number(v.r)||0,g:Number(v.g)||0,b:Number(v.b)||0};
  return {r:0,g:0,b:0};
}
function colorHex(v){
  var c=colorObj(v);
  return '#'+[c.r,c.g,c.b].map(function(x){var h=clampByte(x*255).toString(16);return h.length<2?'0'+h:h}).join('');
}
function colorText(v){
  var c=colorObj(v);
  return formatNumber(c.r)+', '+formatNumber(c.g)+', '+formatNumber(c.b);
}
function colorDisplay(v){return '[color, '+colorText(v)+']'}
function colorFromHex(hex){
  var text=String(hex||'').replace('#','');
  if(text.length!==6)return {_type:'Color3',r:0,g:0,b:0};
  return {_type:'Color3',r:parseInt(text.slice(0,2),16)/255,g:parseInt(text.slice(2,4),16)/255,b:parseInt(text.slice(4,6),16)/255};
}
function colorFromText(text){
  var raw=String(text||'').trim().replace(/^\\[/,'').replace(/\\]$/,'').replace(/^color\\s*,?/i,'');
  if(raw.charAt(0)==='#')return colorFromHex(raw);
  var parts=raw.split(/[\\s,]+/).filter(Boolean).map(Number).filter(function(n){return isFinite(n)});
  return {_type:'Color3',r:parts[0]||0,g:parts[1]||0,b:parts[2]||0};
}
function sliderBounds(info){
  var min=Number(info&&info.uiMinimum),max=Number(info&&info.uiMaximum);
  if(!isFinite(min)||!isFinite(max)||!(max>min))return null;
  var ticks=Number(info&&info.uiNumTicks);
  var dataType=info&&info.dataType?String(info.dataType):'';
  var isInt=/Int/i.test(dataType);
  var step=isFinite(ticks)&&ticks>0?(max-min)/ticks:(isInt?1:0.001);
  if(!isFinite(step)||step<=0)step=isInt?1:0.001;
  if(isInt)step=Math.max(1,Math.round(step));
  return {min:min,max:max,step:step};
}
function valueText(v,dataType){
  var enumType=enumTypeFromDataType(dataType);
  if(enumType){
    var enumName=enumNameFromValue(v,dataType);
    return enumName||'';
  }
  if(dataType==='Color3'||isColor3(v))return colorDisplay(v);
  if(typeof v==='number')return formatNumber(v);
  return typeof v==='string'?v:JSON.stringify(v);
}
function boolFromValue(v){return v===true||String(v).toLowerCase()==='true'}
function wireValueFromValue(v,dataType){
  var enumType=enumTypeFromDataType(dataType);
  if(enumType){
    var enumName=enumNameFromValue(v,dataType);
    return JSON.stringify({_type:'EnumItem',enumType:enumType,name:enumName});
  }
  if(dataType==='Bool'||typeof v==='boolean')return boolFromValue(v)?'true':'false';
  if(dataType==='Color3'||isColor3(v))return JSON.stringify({_type:'Color3',r:colorObj(v).r,g:colorObj(v).g,b:colorObj(v).b});
  return valueText(v,dataType);
}
function wireValueFromInput(input){
  var enumType=input.dataset.enumType;
  if(enumType)return JSON.stringify({_type:'EnumItem',enumType:enumType,name:input.value});
  if(input.dataset.kind==='Color3'){
    var color=input.type==='color'?colorFromHex(input.value):colorFromText(input.value);
    return JSON.stringify(color);
  }
  if(input.type==='checkbox')return input.checked?'true':'false';
  if(input.type==='number'||input.type==='range')return normalizeNumberInputText(input.value);
  return input.value;
}
function readonlyRow(node,scope,name){
  if(scope==='metadata'){
    if(name==='Name')return false;
    if(name==='ClassName')return true;
    if(name==='Parent')return node.kind==='service'||node.locked===true;
    return node.locked===true;
  }
  return false;
}
function editor(scope,name,value,readonly,info){
  var disabled=readonly?' disabled':'';
  var data=' data-scope="'+esc(scope)+'" data-name="'+esc(name)+'"';
  var dataType=info&&info.dataType?String(info.dataType):'';
  var original=esc(wireValueFromValue(value,dataType));
  var enumType=enumTypeFromDataType(dataType);
  if(enumType&&info&&Array.isArray(info.enumItems)){
    var current=enumNameFromValue(value,dataType);
    var options='';
    info.enumItems.forEach(function(item){options+='<option value="'+esc(item)+'"'+(item===current?' selected':'')+'>'+esc(item)+'</option>'});
    return '<select class="value"'+data+' data-original="'+original+'" data-enum-type="'+esc(enumType)+'"'+disabled+'>'+options+'</select>';
  }
  if(dataType==='Color3'||isColor3(value)){
    return '<div class="colorWrap"><input class="color" type="color"'+data+' data-kind="Color3" data-original="'+original+'" value="'+esc(colorHex(value))+'"'+disabled+'><input class="value colorText" type="text" spellcheck="false"'+data+' data-kind="Color3" data-original="'+original+'" value="'+esc(colorDisplay(value))+'"'+disabled+'></div>';
  }
  if(dataType==='Bool'||typeof value==='boolean'){
    return '<input class="bool" type="checkbox"'+data+' data-original="'+original+'"'+(boolFromValue(value)?' checked':'')+disabled+'>';
  }
  if(typeof value==='number'){
    var bounds=sliderBounds(info);
    if(bounds){
      var numberValue=esc(valueText(value,dataType));
      var attrs=' min="'+bounds.min+'" max="'+bounds.max+'" step="'+bounds.step+'"';
      return '<div class="rangeWrap"><input class="value rangeNumber" type="number"'+data+attrs+' data-original="'+original+'" value="'+numberValue+'"'+disabled+'><input class="range" type="range"'+data+attrs+' data-original="'+original+'" value="'+numberValue+'"'+disabled+'></div>';
    }
    return '<input class="value" type="number" step="0.001"'+data+' data-original="'+original+'" value="'+esc(valueText(value,dataType))+'"'+disabled+'>';
  }
  return '<input class="value" type="text" spellcheck="false"'+data+' data-original="'+original+'" value="'+esc(valueText(value,dataType))+'"'+disabled+'>';
}
function row(node,scope,name,value,info){
  var ro=(info&&info.readonly)||readonlyRow(node,scope,name);
  var label=(info&&info.displayName)||name;
  var title=label===name?name:(label+' ('+name+')');
  return '<div class="row'+(ro?' readonly':'')+'"'+(ro?' title="Read-only"':'')+'><div class="key" title="'+esc(title)+'">'+esc(label)+'</div><div class="editor">'+editor(scope,name,value,ro,info)+'</div></div>';
}
function section(title,html){return '<div class="group">'+esc(title)+'</div>'+html}
function categoryRank(category){
  var order={
    Data:0,Camera:1,Character:2,'Character Jump Settings':3,Controls:4,Mobile:5,Permissions:6,
    Behavior:7,Appearance:8,Pivot:9,Transform:10,'Air Properties':11,Avatar:12,Networking:13,
    Physics:14,Pathfinding:15,Rendering:16,Scripting:17,'Server Authority':18,Streaming:19,
    Text:20,Image:21,Layout:22,Localization:23,Tags:24,Attributes:25
  };
  return Object.prototype.hasOwnProperty.call(order,category)?order[category]:99;
}
function render(node){
  if(!node){app.innerHTML='<div id="empty">Select an instance.</div>';return}
  var html='<div class="head"><div class="title">'+esc(node.name)+'</div><div class="class">'+esc(node.className)+'</div></div>';
  var props=node.properties||{};
  var propHtml='';
  var rows=[
    {scope:'metadata',category:'Data',name:'Name',value:node.name},
    {scope:'metadata',category:'Data',name:'ClassName',value:node.className,readonly:true},
    {scope:'metadata',category:'Data',name:'Parent',value:node.parentName||'game'}
  ];
  if(Array.isArray(node.propertyRows)){
    node.propertyRows.forEach(function(info){rows.push(Object.assign({scope:'property'},info))});
  }else{
    Object.keys(props).sort().forEach(function(k){rows.push({scope:'property',category:'Data',name:k,value:props[k]})});
  }
  rows.sort(function(a,b){
    var categorySort=categoryRank(a.category||'Data')-categoryRank(b.category||'Data');
    var orderSort=(Number.isFinite(a.order)?a.order:0)-(Number.isFinite(b.order)?b.order:0);
    return categorySort||String(a.category||'Data').localeCompare(String(b.category||'Data'))||orderSort||String(a.name||'').localeCompare(String(b.name||''));
  });
  if(rows.length){
    var lastCategory=null;
    rows.forEach(function(info){
      var category=info.category||'Data';
      if(category!==lastCategory){propHtml+='<div class="group sub">'+esc(category)+'</div>';lastCategory=category}
      propHtml+=row(node,info.scope||'property',info.name,info.value,info);
    });
  }
  html+=section('Properties',propHtml||'<div id="empty">No stored properties.</div>');
  var attrs=node.attributes||{};
  var attrHtml='';
  Object.keys(attrs).sort().forEach(function(k){if(k.indexOf('RBX_')!==0)attrHtml+=row(node,'attribute',k,attrs[k])});
  html+=section('Attributes',attrHtml||'<div id="empty">No attributes.</div>');
  app.innerHTML=html;
}
function commit(input){
  if(input.disabled)return;
  if(input.type==='number'||input.type==='range')input.value=normalizeNumberInputText(input.value);
  var value=wireValueFromInput(input);
  if(value===input.dataset.original)return;
  vscode.postMessage({type:'setValue',scope:input.dataset.scope,name:input.dataset.name,value:value});
  input.dataset.original=value;
  var wrap=input.closest&&input.closest('.colorWrap,.rangeWrap');
  if(wrap){
    wrap.querySelectorAll('input').forEach(function(peer){peer.dataset.original=value});
  }
}
var liveTimers={};
function liveCommit(input){
  if(!input||input.disabled)return;
  var key=(input.dataset.scope||'')+':'+(input.dataset.name||'');
  clearTimeout(liveTimers[key]);
  liveTimers[key]=setTimeout(function(){commit(input)},120);
}
function syncColorInputs(input){
  if(!input||input.dataset.kind!=='Color3')return;
  var wrap=input.closest('.colorWrap');if(!wrap)return;
  var color=input.type==='color'?colorFromHex(input.value):colorFromText(input.value);
  var picker=wrap.querySelector('input.color'),text=wrap.querySelector('input.colorText');
  if(picker&&input!==picker)picker.value=colorHex(color);
  if(text&&input!==text)text.value=colorDisplay(color);
}
function syncRangeInputs(input){
  if(!input||!input.closest)return;
  var wrap=input.closest('.rangeWrap');if(!wrap)return;
  if(input.type==='range')input.value=normalizeNumberInputText(input.value);
  var peer=input.type==='range'?wrap.querySelector('input.rangeNumber'):wrap.querySelector('input.range');
  if(peer&&peer!==input)peer.value=normalizeNumberInputText(input.value);
}
app.addEventListener('keydown',function(e){
  if(e.key==='Enter'&&e.target&&e.target.tagName==='INPUT'){commit(e.target);e.target.blur()}
});
app.addEventListener('change',function(e){
  if(e.target&&(e.target.tagName==='INPUT'||e.target.tagName==='SELECT')){
    syncColorInputs(e.target);syncRangeInputs(e.target);
    if(e.target.type==='number')e.target.value=normalizeNumberInputText(e.target.value);
    commit(e.target)
  }
});
app.addEventListener('input',function(e){
  if(e.target&&e.target.tagName==='INPUT'){
    syncRangeInputs(e.target);
    if(e.target.type==='range')liveCommit(e.target);
    if(e.target.type==='color'){syncColorInputs(e.target);liveCommit(e.target)}
  }
});
app.addEventListener('focusin',function(e){
  if(e.target&&e.target.classList&&e.target.classList.contains('colorText')){
    e.target.value=String(e.target.value||'').replace(/^\\[/,'').replace(/\\]$/,'');
    e.target.select();
  }
});
app.addEventListener('focusout',function(e){
  if(e.target&&e.target.classList&&e.target.classList.contains('colorText')){
    var color=colorFromText(e.target.value);
    e.target.value=colorDisplay(color);
  }
});
window.addEventListener('message',function(e){if(e.data&&e.data.type==='show')render(e.data.node)});
vscode.postMessage({type:'ready'});
})();
</script>
</body>
</html>`;
  }

}

type ExplorerViewMode = "normal" | "search";

type ExplorerRowSummary = {
  id: string;
  settingsId?: string;
  index?: number;
  kind: FileExplorerNodeKind;
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

type ExplorerBackendResponse = {
  type?: string;
  requestId?: number;
  snapshotVersion?: number;
  viewVersion?: number;
  mode?: ExplorerViewMode;
  start?: number;
  totalRows?: number;
  rows?: ExplorerRowSummary[];
  matchIds?: string[];
  details?: Partial<FileExplorerNode> & {
    id?: string;
    parentId?: string | null;
    settingsId?: string;
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

type ExplorerPendingRequest = {
  resolve: (response: ExplorerBackendResponse) => void;
  reject: (error: Error) => void;
  timer: NodeJS.Timeout;
};

type ExplorerRowRequest = {
  start: number;
  count: number;
  mode: ExplorerViewMode;
  scrollToSelected: boolean;
  includeMatchIds: boolean;
  revision?: number;
};

class ExplorerBackendClient implements vscode.Disposable {
  private process: childProcess.ChildProcessWithoutNullStreams | undefined;
  private buffer = "";
  private requestId = 1;
  private readonly pending = new Map<number, ExplorerPendingRequest>();
  private disposed = false;
  private starting: Promise<void> | undefined;
  private initialized = false;
  private processInitialized = false;

  public constructor(private readonly onEvent: (response: ExplorerBackendResponse) => void) {}

  public dispose(): void {
    this.disposed = true;
    for (const [id, pending] of this.pending) {
      clearTimeout(pending.timer);
      pending.reject(new Error(`Explorer backend request ${id} cancelled.`));
    }
    this.pending.clear();
    if (this.process && !this.process.killed) {
      try {
        this.process.stdin.write(`${JSON.stringify({ t: "quit", id: this.requestId++ })}\n`);
      } catch {
      }
      this.process.kill();
    }
    this.process = undefined;
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
    if (this.process && !this.process.killed) {
      this.process.kill();
    }
    this.process = undefined;
  }

  public async getRows(start: number, count: number, mode: ExplorerViewMode, includeMatchIds = false): Promise<ExplorerBackendResponse> {
    return this.request(mode === "search" ? "searchRows" : "getRows", { start, count, mode, includeMatchIds });
  }

  public async expand(nodeId: string, mode: ExplorerViewMode): Promise<ExplorerBackendResponse> {
    return this.request("expand", { nodeId, mode });
  }

  public async collapse(nodeId: string, mode: ExplorerViewMode): Promise<ExplorerBackendResponse> {
    return this.request("collapse", { nodeId, mode });
  }

  public async selectDetails(nodeId: string): Promise<ExplorerBackendResponse> {
    return this.request("selectDetails", { nodeId });
  }

  public async searchStart(query: string, searchId: number): Promise<ExplorerBackendResponse> {
    return this.request("searchStart", { query, searchId });
  }

  public async clearSearch(): Promise<ExplorerBackendResponse> {
    return this.request("clearSearch", {});
  }

  public async reloadServices(services: string[]): Promise<ExplorerBackendResponse> {
    return this.request("reloadServices", { services });
  }

  public async revealNode(nodeId: string): Promise<ExplorerBackendResponse> {
    return this.request("revealNode", { nodeId });
  }

  private async request(type: string, payload: Record<string, unknown>): Promise<ExplorerBackendResponse> {
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

  private async requestOnce(type: string, payload: Record<string, unknown>): Promise<ExplorerBackendResponse> {
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
      const timeoutMs = type === "searchStart" || type === "searchRows" ? 10000 : 30000;
      const timer = setTimeout(() => {
        this.pending.delete(requestId);
        if (this.process && !this.process.killed) {
          this.process.kill();
          this.process = undefined;
          this.processInitialized = false;
        }
        reject(new Error(`Explorer backend request timed out: ${type}`));
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

  private encodeRequest(type: string, requestId: number, payload: Record<string, unknown>): Record<string, unknown> {
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
      shutdown: "quit",
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
      cancelRequestId: "cid",
    };
    const message: Record<string, unknown> = {
      t: typeMap[type] ?? type,
      id: requestId,
    };
    for (const [key, value] of Object.entries(payload)) {
      message[keyMap[key] ?? key] = value;
    }
    return message;
  }

  private isRestartableError(error: unknown): boolean {
    const message = error instanceof Error ? error.message : String(error);
    return message.includes("Explorer backend exited") || message.includes("Explorer backend request timed out") || message.includes("not running");
  }

  private async ensureStarted(): Promise<void> {
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
    const config = getExplorerConfig();
    if (!fs.existsSync(config.rustCliPath)) {
      throw new Error(`${RUST_CLI_BINARY} was not found at ${config.rustCliPath}. Build the CLI or point the "renium.rustCliPath" setting at it.`);
    }
    const child = childProcess.spawn(config.rustCliPath, [
      "ed",
      "-r",
      config.projectRoot,
      "-d",
      "src",
      "-s",
      config.services.join(","),
      "--parent-pid",
      String(process.pid),
    ], {
      cwd: config.projectRoot,
      env: process.env,
      shell: false,
      stdio: "pipe",
      windowsHide: true,
    });
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
        this.failAll(new Error(`Explorer backend exited with code ${code ?? 0}.`));
      }
    });
  }

  private handleStdout(chunk: string): void {
    this.buffer += chunk;
    for (;;) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) {
        break;
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

class FileExplorerViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = "renium.fileExplorer";
  private webviewView: vscode.WebviewView | undefined;
  private selectedId: string | undefined;
  private webviewReady = false;
  private lastErrorMessage: string | undefined;
  private readonly backend = new ExplorerBackendClient((response) => this.onBackendEvent(response));
  private readonly searchBackend = new ExplorerBackendClient((response) => this.onBackendEvent(response));
  private clipboardNodeId: string | undefined;
  private currentMode: ExplorerViewMode = "normal";
  private rowWindow = { start: 0, count: 80 };
  private rowRequestSerial = 0;
  private rowRequestInFlight = false;
  private queuedRowRequest: ExplorerRowRequest | undefined;
  private mutationQueue: Promise<void> = Promise.resolve();
  private searchGeneration = 0;
  private readonly propertyOnlyStaleServices = new Set<string>();
  private referencePreviewId: string | undefined;
  private referencePreviewScrollPending = false;
  private readonly availableIconNames: ReadonlySet<string>;
  private gitState: GitViewState | undefined;
  private gitLoading = false;
  private revealGitOnReady = false;
  private externalPackageDrag: { id: string; name?: string; mode?: string } | undefined;
  private packageCursorProcess: childProcess.ChildProcess | undefined;
  private packageCursorBuffer = "";
  private packageCursorLastPost = 0;
  private packageCursorLastTrace = 0;
  private packageCursorSampleCount = 0;
  private packageCursorSawButtonDown = false;
  private packageCursorReleaseTimer: NodeJS.Timeout | undefined;
  private linkState: Record<string, string> = {};

  public setLinkState(keys: Record<string, string>): void {
    this.linkState = keys || {};
    if (this.webviewReady) {
      this.webviewView?.webview.postMessage({ type: "linkState", keys: this.linkState });
    }
  }

  public setExternalPackageDrag(link?: { id: string; name?: string; mode?: string }): void {
    this.externalPackageDrag = link;
    logPackageDragDebug(
      `explorer.host.setExternalPackageDrag: ${link ? `armed ${link.id} name=${link.name ?? ""} mode=${link.mode ?? ""}` : "cleared"} webviewReady=${this.webviewReady}`,
    );
    if (link && packageDragDebugOutput) {
      packageDragDebugOutput.show(true);
    }
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
    const cursorPollCliPath = resolveExplorerRustCliPathForCommand(root, config.rustCliPath, "cursor-poll");
    const child = childProcess.spawn(
      cursorPollCliPath,
      ["cursor-poll", "--interval-ms", "16"],
      { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] },
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
        child.kill();
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
    child.kill();
  }

  private nodeTargetPath(node: FileExplorerNode): string[] {
    return node.pathSegments.length > 0 ? node.pathSegments.slice() : [node.service, node.name];
  }

  private nodeLinkPathKey(node: FileExplorerNode): string | undefined {
    const pathSegments = this.nodeTargetPath(node);
    if (pathSegments.length < 2) {
      return undefined;
    }
    return `${pathSegments[0]}\u0001${pathSegments.slice(1).join("/")}`;
  }

  private linkPathKey(service: string, pathSegments: string[]): string | undefined {
    const segments = pathSegments.length > 0 ? pathSegments : [service];
    const normalized = segments[0] === service ? segments : [service, ...segments];
    if (normalized.length < 2) {
      return undefined;
    }
    return `${normalized[0]}\u0001${normalized.slice(1).join("/")}`;
  }

  private directReniumLinkTargetPath(node: FileExplorerNode): string[] | undefined {
    const key = this.nodeLinkPathKey(node);
    return key && this.linkState[key] === "linked" ? this.nodeTargetPath(node) : undefined;
  }

  private childPathUnder(parent: FileExplorerNode, childName: string): string[] {
    const parentPath = parent.kind === "service" ? [parent.service] : this.nodeTargetPath(parent);
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
    const currentPath = this.nodeTargetPath(node);
    if (currentPath.length > 0) {
      return currentPath.slice(0, -1).concat(newName);
    }
    return [node.service, newName];
  }

  private hasLinkedTargetCollision(service: string, oldPathSegments: string[], newPathSegments: string[]): boolean {
    const oldKey = this.linkPathKey(service, oldPathSegments);
    const newKey = this.linkPathKey(service, newPathSegments);
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
    const oldKey = this.linkPathKey(oldService, oldPathSegments);
    const newKey = this.linkPathKey(newService, newPathSegments);
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
      void this.onMessage(message);
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
    this.referencePreviewId = nodeId;
    this.referencePreviewScrollPending = true;
    this.currentMode = "normal";
    this.queuedRowRequest = undefined;
    this.rowRequestSerial += 1;
    this.reveal();
    this.webviewView?.webview.postMessage({ type: "prepareReferencePreview" });
    await this.backend.ensureInitialized();
    const count = Math.max(this.rowWindow.count, 120);
    let start = this.rowWindow.start;
    const reveal = await this.backend.revealNode(nodeId);
    if (typeof reveal.rowIndex === "number") {
      start = Math.max(0, reveal.rowIndex - Math.floor(count / 2));
    }
    this.rowWindow = { start, count };
    await this.requestRows(start, count, "normal");
  }

  public async refresh(): Promise<void> {
    try {
      await this.backend.initialize();
      await this.requestRows();
      void this.searchBackend.ensureInitialized().catch(() => undefined);
    } catch (error) {
      this.pushError(error instanceof Error ? error.message : String(error));
      vscode.window.showErrorMessage(`Renium: failed to refresh Explorer. ${error instanceof Error ? error.message : String(error)}`);
    }
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
    this.gitLoading = true;
    this.postGitState();
    try {
      this.gitState = await git.refresh(options);
    } catch (error) {
      this.gitState = {
        ok: false,
        message: error instanceof Error ? error.message : String(error),
        trusted: vscode.workspace.isTrusted,
        connected: false,
        ahead: 0,
        behind: 0,
        counts: { total: 0, tracked: 0, staged: 0, unstaged: 0, untracked: 0, ignored: 0, conflicted: 0, deleted: 0 },
        entries: [],
        lastUpdated: new Date().toISOString(),
      };
    } finally {
      this.gitLoading = false;
      this.postGitState();
    }
  }

  private postGitState(): void {
    if (!this.webviewView || !this.webviewReady) {
      return;
    }
    this.webviewView.webview.postMessage({ type: "gitState", state: this.gitState, loading: this.gitLoading });
  }

  public async refreshServices(services: string[]): Promise<void> {
    try {
      await this.backend.reloadServices(services);
      if (this.searchBackend.hasInitialized()) {
        await this.searchBackend.reloadServices(services).catch(() => undefined);
      }
      for (const service of services) {
        this.propertyOnlyStaleServices.delete(service);
      }
      try {
        await this.propertiesProvider.refreshCurrentForServices(services);
      } catch (error) {
        if (!isNoMatchingInstanceError(error)) {
          throw error;
        }
      }
      await this.requestRows();
    } catch (error) {
      if (isNoMatchingInstanceError(error)) {
        await this.requestRows().catch(() => undefined);
        return;
      }
      this.pushError(error instanceof Error ? error.message : String(error));
      vscode.window.showErrorMessage(`Renium: failed to refresh Explorer changes. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async enqueueMutation(task: () => Promise<void>): Promise<void> {
    const run = this.mutationQueue.then(task, task);
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
    };
    if (this.rowRequestInFlight) {
      return;
    }
    this.rowRequestInFlight = true;
    try {
      while (this.queuedRowRequest) {
        const request: ExplorerRowRequest = this.queuedRowRequest;
        this.queuedRowRequest = undefined;
        this.rowWindow = { start: request.start, count: request.count };
        const serial = ++this.rowRequestSerial;
        try {
          const backend = request.mode === "search" ? this.searchBackend : this.backend;
          if (request.mode === "search") {
            await backend.ensureInitialized();
          }
          const response = await backend.getRows(request.start, request.count, request.mode, request.includeMatchIds);
          if (this.queuedRowRequest || serial !== this.rowRequestSerial || request.mode !== this.currentMode) {
            if (request.mode === this.currentMode) {
              this.postRowsPrefetch(response, request.revision);
            }
            continue;
          }
          this.postRowsWindow(response, request.scrollToSelected, request.revision);
        } catch (error) {
          if (!this.queuedRowRequest && request.mode === this.currentMode) {
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
    try {
      const backend = mode === "search" ? this.searchBackend : this.backend;
      if (mode === "search") {
        await backend.ensureInitialized();
      }
      const response = await backend.getRows(Math.max(0, start), Math.max(1, Math.min(2400, count)), mode);
      if (!this.webviewView || !this.webviewReady || mode !== this.currentMode || response.type !== "rowsWindow") {
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
      this.webviewView?.webview.postMessage({ type: "rowsPrefetchDone", mode, revision });
    }
  }

  private nodeFromBackend(row: ExplorerRowSummary | ExplorerBackendResponse["details"]): FileExplorerNode {
    const id = String(row?.id ?? "");
    const service = String(row?.service ?? this.serviceFromNodeId(id) ?? "");
    const kind = (row?.kind === "service" ? "service" : "instance") as FileExplorerNodeKind;
    const settingsId = typeof row?.settingsId === "string" ? row.settingsId : this.settingsIdFromNodeId(id);
    const config = getExplorerConfig();
    return {
      id,
      treeId: id,
      kind,
      service,
      name: String(row?.name ?? service),
      className: String(row?.className ?? service),
      settingsId,
      index: typeof row?.index === "number" ? row.index : undefined,
      parentTreeId: typeof row?.parentId === "string" ? row.parentId : null,
      children: [],
      loaded: false,
      detailsLoaded: !!row?.properties || !!row?.attributes,
      hasChildren: row?.hasChildren === true || Number(row?.childCount ?? 0) > 0,
      hasPackageLink: row?.hasPackageLink === true,
      settingsFile: settingsFileForService(config, service),
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

  /** Decode raw bytes dropped from the OS onto the Inspector tab. */
  private async handleRbsyncDecode(name: string | undefined, base64: string | undefined): Promise<void> {
    const webview = this.webviewView?.webview;
    if (!webview || typeof base64 !== "string") {
      return;
    }
    const displayName = name && name.trim().length > 0 ? name.trim().slice(0, 255) : "dropped.renium";
    if (base64.length > MAX_RBSYNC_DROPPED_BASE64_CHARS || base64.length % 4 === 1 || !/^[A-Za-z0-9+/]*={0,2}$/.test(base64)) {
      this.postRbsyncResult(webview, displayName, {
        ok: false,
        error: `Dropped files must be valid base64 and no larger than ${Math.floor(MAX_RBSYNC_DROPPED_BYTES / (1024 * 1024))} MiB.`,
      });
      return;
    }
    const bytes = Buffer.from(base64, "base64");
    if (bytes.length > MAX_RBSYNC_DROPPED_BYTES) {
      this.postRbsyncResult(webview, displayName, {
        ok: false,
        error: `Dropped files are limited to ${Math.floor(MAX_RBSYNC_DROPPED_BYTES / (1024 * 1024))} MiB. Use the file picker for a larger store.`,
      });
      return;
    }
    const config = getExplorerConfig();
    this.postRbsyncResult(
      webview,
      displayName,
      await decodeRbsyncBytes(config.rustCliPath, config.projectRoot, bytes),
    );
  }

  /** Decode a file dropped from the VS Code Explorer (a text/uri-list URI) or
   * chosen via the browse dialog — both arrive as a filesystem path/URI. */
  private async handleRbsyncDecodePath(raw: string | undefined): Promise<void> {
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
      /* fall back to the raw string as a path */
    }
    const config = getExplorerConfig();
    this.postRbsyncResult(
      webview,
      path.basename(fsPath) || "store.renium",
      await decodeRbsyncToTree(config.rustCliPath, config.projectRoot, fsPath),
    );
  }

  /** Browse fallback so the tab is usable even where webview file drops don't
   * fire: open a picker and decode the chosen .renium store. */
  private async handleRbsyncBrowse(): Promise<void> {
    const picked = await vscode.window.showOpenDialog({
      canSelectMany: false,
      openLabel: "Inspect",
      filters: { "Renium store": ["renium", "rbsync"], "All files": ["*"] },
    });
    if (picked && picked[0]) {
      await this.handleRbsyncDecodePath(picked[0].fsPath);
    }
  }

  private postRbsyncResult(webview: vscode.Webview, displayName: string, result: DecodeResult): void {
    if (!result.ok) {
      void webview.postMessage({ type: "rbsyncTree", name: displayName, error: result.error });
      return;
    }
    void webview.postMessage({ type: "rbsyncTree", name: displayName, result: result.tree });
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
    message?: string;
    base64?: string;
    node?: {
      name?: string;
      className?: string;
      settingsId?: string;
      properties?: Record<string, unknown>;
      attributes?: Record<string, unknown>;
      pathSegments?: string[];
    };
  }): Promise<void> {
    const node = message.nodeId ? this.model.getNode(message.nodeId) : undefined;
    switch (message.type) {
      case "rbsyncDecode":
        await this.handleRbsyncDecode(message.name, message.base64);
        break;
      case "rbsyncDecodePath":
        await this.handleRbsyncDecodePath(message.path);
        break;
      case "rbsyncBrowse":
        await this.handleRbsyncBrowse();
        break;
      case "rbsyncSelect":
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
        await this.refreshGit({ fetch: message.fetch === true });
        return;
      case "gitAction":
        if (this.actions.git && message.action) {
          this.gitLoading = true;
          this.postGitState();
          try {
            await this.actions.git.runAction(String(message.action));
          } finally {
            await this.refreshGit();
          }
        }
        return;
      case "gitOpenOutput":
        this.actions.git?.openOutput();
        return;
      case "gitDiff":
        if (this.actions.git && message.path) {
          await this.actions.git.openDiff(String(message.path));
        }
        return;
      case "loadHistory":
        await this.postHistoryEntries();
        return;
      case "openHistoryBackup":
        try {
          await this.openHistoryBackup(message.historyId);
        } catch (error) {
          vscode.window.showErrorMessage(`Renium: failed to open history backup. ${error instanceof Error ? error.message : String(error)}`);
        }
        return;
      case "compareHistoryBackup":
        try {
          await this.compareHistoryBackup(message.historyId);
        } catch (error) {
          vscode.window.showErrorMessage(`Renium: failed to compare history backup. ${error instanceof Error ? error.message : String(error)}`);
        }
        return;
      case "restoreHistory":
        try {
          await this.restoreHistoryEntry(message.historyId);
        } catch (error) {
          this.webviewView?.webview.postMessage({ type: "historyRestoreComplete", id: message.historyId });
          vscode.window.showErrorMessage(`Renium: failed to restore history. ${error instanceof Error ? error.message : String(error)}`);
        }
        return;
      case "restoreHistoryGroup":
        try {
          await this.restoreHistoryGroup(message.historyIds, message.historyGroupId);
        } catch (error) {
          this.webviewView?.webview.postMessage({ type: "historyRestoreComplete", groupId: message.historyGroupId });
          vscode.window.showErrorMessage(`Renium: failed to restore history group. ${error instanceof Error ? error.message : String(error)}`);
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
          this.currentMode = message.mode ?? this.currentMode;
          try {
            const backend = this.currentMode === "search" ? this.searchBackend : this.backend;
            if (this.currentMode === "search") {
              await backend.ensureInitialized();
            }
            await backend.expand(message.nodeId, this.currentMode);
            await this.requestRows(message.start ?? this.rowWindow.start, message.count ?? this.rowWindow.count, this.currentMode);
          } catch (error) {
            this.webviewView?.webview.postMessage({ type: "loadComplete", nodeId: message.nodeId, ok: false });
            vscode.window.showErrorMessage(`Renium: failed to expand instance. ${error instanceof Error ? error.message : String(error)}`);
          }
        }
        return;
      case "collapseNode":
        if (message.nodeId) {
          this.currentMode = message.mode ?? this.currentMode;
          try {
            const backend = this.currentMode === "search" ? this.searchBackend : this.backend;
            if (this.currentMode === "search") {
              await backend.ensureInitialized();
            }
            await backend.collapse(message.nodeId, this.currentMode);
            await this.requestRows(message.start ?? this.rowWindow.start, message.count ?? this.rowWindow.count, this.currentMode);
          } catch (error) {
            vscode.window.showErrorMessage(`Renium: failed to collapse instance. ${error instanceof Error ? error.message : String(error)}`);
          }
        }
        return;
      case "selectNode":
        if (message.nodeId) {
          this.referencePreviewId = undefined;
          this.selectedId = message.nodeId;
          const service = this.serviceFromNodeId(message.nodeId);
          let loadedNode: FileExplorerNode;
          if (service && this.propertyOnlyStaleServices.has(service)) {
            loadedNode = await this.model.loadDetails(this.model.getNode(message.nodeId) ?? this.nodeFromBackend({ id: message.nodeId }));
          } else {
            const details = await this.backend.selectDetails(message.nodeId);
            loadedNode = this.model.rememberNode(this.nodeFromBackend(details.details ?? { id: message.nodeId }));
          }
          await this.propertiesProvider.show(loadedNode);
          this.actions.onSelectNode?.();
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
    const generation = ++this.searchGeneration;
    const searchId = generation;
    const trimmedQuery = query.trim();
    this.currentMode = trimmedQuery ? "search" : "normal";
    this.queuedRowRequest = undefined;
    this.rowRequestSerial += 1;
    try {
      if (!trimmedQuery) {
        await this.backend.clearSearch();
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
          if (generation === this.searchGeneration) {
            const firstCount = Math.max(700, Math.min(1800, count ?? this.rowWindow.count));
            const searchRows = await this.searchBackend.getRows(0, firstCount, "search");
            if (generation === this.searchGeneration && this.currentMode === "search") {
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
          if (generation !== this.searchGeneration) {
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
      if (generation === this.searchGeneration) {
        this.webviewView?.webview.postMessage({ type: "searchStatus", loading: false });
        vscode.window.showErrorMessage(`Renium: failed to load search results. ${error instanceof Error ? error.message : String(error)}`);
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
      vscode.window.showErrorMessage(`Renium: failed to add instance. ${error instanceof Error ? error.message : String(error)}`);
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
          `Renium: linked package targets need unique sibling names. ${newName} already exists under ${parent?.name ?? loaded.service}.`,
        );
        return;
      }
      if (oldLinkTargetPath && newLinkTargetPath && this.hasLinkedTargetCollision(loaded.service, oldLinkTargetPath, newLinkTargetPath)) {
        vscode.window.showWarningMessage(
          `Renium: ${newName} is already a linked package target under this parent. Rename one of them to a unique name before linking or deleting.`,
        );
        return;
      }
      const renamed = await this.model.renameInstance(loaded, newName);
      if (oldLinkTargetPath && newLinkTargetPath) {
        await this.moveReniumLinkTarget(loaded.service, oldLinkTargetPath, loaded.service, newLinkTargetPath);
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
      vscode.window.showErrorMessage(`Renium: failed to rename instance. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async moveInstance(node: FileExplorerNode, target: FileExplorerNode): Promise<void> {
    try {
      const loaded = await this.model.ensureLoaded(node);
      const loadedTarget = await this.model.ensureLoaded(target);
      const oldLinkTargetPath = this.directReniumLinkTargetPath(loaded);
      if (oldLinkTargetPath && this.siblingNamed(loadedTarget, loaded.name, loaded.treeId)) {
        vscode.window.showWarningMessage(
          `Renium: linked package targets need unique sibling names. ${loaded.name} already exists under ${loadedTarget.name}.`,
        );
        return;
      }
      const newLinkTargetPath = oldLinkTargetPath ? this.childPathUnder(loadedTarget, loaded.name) : undefined;
      const moved = await this.model.moveInstance(loaded, loadedTarget);
      if (oldLinkTargetPath && newLinkTargetPath) {
        await this.moveReniumLinkTarget(loaded.service, oldLinkTargetPath, loadedTarget.service, newLinkTargetPath);
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
    } catch (error) {
      vscode.window.showErrorMessage(`Renium: failed to move instance. ${error instanceof Error ? error.message : String(error)}`);
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
      vscode.window.showWarningMessage("Renium: copied instance no longer exists.");
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
      vscode.window.showErrorMessage(`Renium: failed to paste instance. ${error instanceof Error ? error.message : String(error)}`);
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
      vscode.window.showErrorMessage(`Renium: failed to duplicate instance. ${error instanceof Error ? error.message : String(error)}`);
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
      vscode.window.showWarningMessage("Renium: history source backup was not found.");
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
      vscode.window.showWarningMessage("Renium: history source backup was not found.");
      return;
    }
    if (!pathInsideRoot(config.projectRoot, currentPath) || !fs.existsSync(currentPath)) {
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
      vscode.window.showWarningMessage("Renium: history entry was not found.");
      return;
    }
    const manifest = data.manifest;
    const service = String(manifest.service ?? "").trim();
    const sourcePath = typeof manifest.sourcePath === "string" ? manifest.sourcePath : undefined;
    const settingsId = typeof manifest.settingsId === "string" ? manifest.settingsId : undefined;
    if (!service || (!sourcePath && !settingsId)) {
      vscode.window.showWarningMessage("Renium: this history entry can't be restored — it no longer points at a file or instance.");
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
      vscode.window.showWarningMessage("Renium: history group has no restore targets.");
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
        const from = path.join(data.entryDir, manifest.settingsBackup);
        if (pathInsideRoot(historyRoot, from) && fs.existsSync(from)) {
          const to = settingsFileForService(config, service);
          fs.mkdirSync(path.dirname(to), { recursive: true });
          fs.copyFileSync(from, to);
          changedPaths.push(to);
        }
      }

      if (typeof manifest.sourceBackup === "string" && sourcePath) {
        const from = path.join(data.entryDir, manifest.sourceBackup);
        const to = path.isAbsolute(sourcePath) ? path.normalize(sourcePath) : path.normalize(path.join(config.projectRoot, sourcePath));
        if (!pathInsideRoot(historyRoot, from) || !fs.existsSync(from)) {
          continue;
        }
        if (!pathInsideRoot(config.projectRoot, to)) {
          throw new Error(`Refusing to restore history outside project root: ${to}`);
        }
        fs.mkdirSync(path.dirname(to), { recursive: true });
        fs.copyFileSync(from, to);
        changedPaths.push(to);
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
    vscode.window.showInformationMessage(isGroup ? "Renium: history session restored locally." : "Renium: history entry restored locally.");

    if (changedPaths.length > 0) {
      const uniqueChangedPaths = Array.from(new Set(changedPaths));
      void vscode.commands.executeCommand("renium.pushEditorPathsNow", uniqueChangedPaths, {
        skipChangeFilter: true,
        taskName: "History restore -> Studio sync",
      }).then(
        undefined,
        (error) => vscode.window.showErrorMessage(`Renium: failed to push restored history. ${error instanceof Error ? error.message : String(error)}`),
      );
    }
  }

  private html(assetBase: string): string {
    const classNamesJson = JSON.stringify(ROBLOX_CLASS_NAMES);
    const assetIconNamesJson = JSON.stringify(Array.from(this.availableIconNames));
    const initialRows = this.staticRootRows(assetBase);
    return `<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src *; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
*{box-sizing:border-box}
:root{--property-editor-focus-border:rgba(128,128,128,.45)}
html,body{height:100%;margin:0;overflow:hidden;font-family:var(--vscode-font-family);font-size:var(--vscode-font-size);color:var(--vscode-sideBar-foreground);background:var(--vscode-sideBar-background)}
body{position:relative;display:flex;flex-direction:column}
#tabs{height:30px;display:flex;align-items:end;justify-content:center;padding:3px 4px 0;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent);gap:2px}
.tabBtn{height:26px;flex:1 1 0;min-width:0;max-width:110px;box-sizing:border-box;border:0;background:transparent;color:var(--vscode-descriptionForeground);padding:0 6px;cursor:pointer;font:inherit;border-radius:3px 3px 0 0;text-align:center;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.tabBtn:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground));color:var(--vscode-foreground)}
.tabBtn.active{background:var(--vscode-list-activeSelectionBackground);color:var(--vscode-list-activeSelectionForeground)}
#explorerPane,#historyPane,#gitPane{flex:1;min-height:0;display:flex;flex-direction:column}
#explorerPane{position:relative}
.hidden{display:none!important}
#bar{height:30px;display:flex;align-items:center;gap:6px;padding:4px;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent)}
#search{flex:1;min-width:0;height:22px;border:1px solid var(--vscode-input-border,transparent);background:var(--vscode-input-background);color:var(--vscode-input-foreground);padding:2px 5px;font:inherit}
#searchMeta{height:24px;display:none;align-items:center;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent);padding:2px 4px;color:var(--vscode-descriptionForeground)}
#searchMeta.active{display:flex}
.searchSummary{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.searchActions{display:flex;align-items:center;gap:2px}
.iconBtn{width:22px;height:20px;border:0;background:transparent;color:var(--vscode-icon-foreground);padding:0;cursor:pointer;font:inherit}
.iconBtn:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground))}
#suggestions{display:none;position:absolute;z-index:5357;top:30px;left:0;right:0;max-height:min(320px,calc(100% - 30px));overflow:auto;border:1px solid var(--vscode-sideBarSectionHeader-border,transparent);border-top:0;background:var(--vscode-sideBar-background);padding:7px 12px 8px 12px;color:var(--vscode-foreground);box-shadow:0 8px 18px rgba(0,0,0,.22)}
#suggestions.active{display:block}
.suggestTitle{color:var(--vscode-descriptionForeground);margin-bottom:6px}
.suggestItem{height:22px;display:flex;align-items:center;gap:8px;white-space:nowrap;cursor:pointer;margin:0 -6px;padding:0 6px;border-radius:2px;transition:background-color .1s ease,color .1s ease}
.suggestItem:hover{background:var(--vscode-list-hoverBackground);background:color-mix(in srgb,var(--vscode-list-hoverBackground) 72%,white 18%);color:var(--vscode-foreground)}
.suggestItem:hover .suggestIcon{color:var(--vscode-foreground)}
.suggestIcon{width:16px;text-align:center;color:var(--vscode-descriptionForeground);transition:color .1s ease}
#tree{flex:1;min-height:0;overflow:auto;outline:none;padding:2px 0}
#treeEmpty{padding:8px;color:var(--vscode-descriptionForeground)}
.row{height:22px;display:flex;align-items:center;white-space:nowrap;cursor:pointer;user-select:none;outline:1px solid transparent;line-height:22px}
.row:hover{background:var(--vscode-list-hoverBackground)}
.row.selected{background:var(--vscode-list-activeSelectionBackground);color:var(--vscode-list-activeSelectionForeground)}
.row.match-selected:not(.selected){background:var(--vscode-list-inactiveSelectionBackground)}
.row.reference-preview:not(.selected){background:var(--vscode-list-inactiveSelectionBackground,var(--vscode-list-hoverBackground));box-shadow:inset 0 0 0 1px var(--property-editor-focus-border)}
.row.disabled .name,.row.disabled .class,.row.disabled .icon{opacity:.45}
.row.dragging{opacity:.45}
.row.drop-target{outline:2px solid var(--vscode-focusBorder);outline-offset:-2px;background:var(--vscode-list-dropBackground,var(--vscode-list-hoverBackground));box-shadow:inset 4px 0 0 var(--vscode-focusBorder)}
.row.renium-linked{box-shadow:inset 3px 0 0 var(--vscode-charts-blue,var(--vscode-focusBorder))}
.row.renium-package{box-shadow:inset 3px 0 0 var(--vscode-charts-purple,var(--vscode-focusBorder))}
.row.renium-broken{box-shadow:inset 3px 0 0 var(--vscode-editorWarning-foreground,var(--vscode-charts-yellow))}
.row.placeholder{color:var(--vscode-descriptionForeground)}
.twisty{width:16px;height:22px;display:flex;align-items:center;justify-content:center;font-size:9px;opacity:1;color:#fff}
.twisty::before{content:'\\25B6'}
.twisty.open::before{transform:rotate(90deg)}
.twisty.leaf{visibility:hidden}
.icon{width:16px;height:16px;flex:0 0 16px;margin-right:4px;display:block;object-fit:contain;object-position:center center;image-rendering:pixelated}
.labelWrap{display:inline-flex;align-items:center;min-width:0;flex:1 1 auto;overflow:hidden}
.name{display:flex;align-items:center;min-width:0;max-width:100%;height:22px;line-height:22px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;flex:0 1 auto}
.linkBadge{margin-left:6px;box-sizing:border-box;height:16px;line-height:14px;display:inline-flex;align-items:center;transform:translateY(1px);border-radius:999px;border:1px solid currentColor;padding:0 5px;font-size:10px;font-weight:600;letter-spacing:.02em;opacity:.9;flex:0 0 auto}
.linkBadge.linked{color:var(--vscode-charts-blue,var(--vscode-focusBorder))}
.linkBadge.package{color:var(--vscode-charts-purple,var(--vscode-focusBorder))}
.linkBadge.broken{color:var(--vscode-editorWarning-foreground,var(--vscode-charts-yellow))}
.addBtn{margin-left:6px;flex:none;position:relative;width:18px;height:18px;display:inline-flex;align-items:center;justify-content:center;box-sizing:border-box;padding:0;border-radius:999px;border:1px solid var(--vscode-descriptionForeground);background:color-mix(in srgb,var(--vscode-sideBar-background) 88%,white 12%);color:var(--vscode-foreground);font-size:0;line-height:0;appearance:none;opacity:0;pointer-events:none;transform:scale(.92);transition:opacity .12s ease,transform .12s ease,background-color .12s ease,border-color .12s ease}
.addBtn::before{content:'+';font:600 14px/1 var(--vscode-font-family);transform:translateY(-.5px)}
.addBtn:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground));border-color:var(--vscode-focusBorder)}
.row:hover .addBtn{opacity:1;pointer-events:auto;transform:scale(1)}
.rename{height:20px;min-width:80px;width:160px;border:1px solid var(--vscode-input-border,transparent);background:var(--vscode-input-background);color:var(--vscode-input-foreground);font:inherit;line-height:18px;padding:1px 4px}
.class{margin-left:6px;color:var(--vscode-descriptionForeground);overflow:hidden;text-overflow:ellipsis}
#menu{position:fixed;z-index:10;min-width:150px;border:1px solid var(--vscode-menu-border);background:var(--vscode-menu-background);padding:4px 0}
#menu.hidden{display:none}
.mi{padding:4px 10px;cursor:pointer;color:var(--vscode-menu-foreground)}
.mi:hover{background:var(--vscode-menu-selectionBackground);color:var(--vscode-menu-selectionForeground)}
#classPicker{position:fixed;z-index:11;width:240px;max-height:320px;border:1px solid var(--vscode-menu-border);background:var(--vscode-menu-background);box-shadow:0 4px 14px rgba(0,0,0,.25);padding:6px}
#classPicker.hidden{display:none}
#classSearch{width:100%;height:22px;margin-bottom:6px;border:1px solid var(--vscode-input-border,transparent);background:var(--vscode-input-background);color:var(--vscode-input-foreground);font:inherit;padding:2px 5px}
#search:focus,#search:focus-visible,#classSearch:focus,#classSearch:focus-visible,.rename:focus,.rename:focus-visible{border-color:var(--property-editor-focus-border)!important;background:var(--vscode-input-background)!important;outline:none!important;box-shadow:none!important}
#classList{max-height:260px;overflow:auto}
.classItem{height:22px;display:flex;align-items:center;gap:6px;padding:2px 4px;cursor:pointer;color:var(--vscode-menu-foreground)}
.classItem:hover,.classItem.active{background:var(--vscode-menu-selectionBackground);color:var(--vscode-menu-selectionForeground)}
#historyHeader{height:30px;display:flex;align-items:center;gap:6px;padding:4px;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent)}
#historyTitle{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--vscode-descriptionForeground)}
#historyList{flex:1;min-height:0;overflow:auto;padding:2px 0}
.historyGroup{position:relative;--history-guide-color:var(--vscode-tree-indentGuidesStroke,var(--vscode-editorIndentGuide-background,rgba(128,128,128,.35)));--history-guide-x:22px;--history-children-indent:23px;--history-connector-length:11px;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent)}
.historyGroupHeader{position:relative;display:grid;grid-template-columns:18px minmax(0,1fr) max-content;gap:4px;align-items:center;min-height:42px;padding:4px 6px;cursor:pointer}
.historyGroupHeader:hover,.historyChild:hover{background:var(--vscode-list-hoverBackground)}
.historyTwisty{width:18px;height:22px;border:0;background:transparent;color:var(--vscode-icon-foreground);padding:0;cursor:pointer;font:inherit}
.historyTwisty::before{content:'\\25B6';font-size:9px}
.historyGroup.open .historyTwisty::before{display:inline-block;transform:rotate(90deg)}
.historyMain,.historyChildMain{min-width:0}
.historyChildren{padding:0 0 4px var(--history-children-indent)}
.historyGroup.open .historyGroupHeader::after{content:'';position:absolute;left:var(--history-guide-x);top:22px;bottom:-17px;width:1px;background:var(--history-guide-color);opacity:.72;pointer-events:none}
.historyChild{position:relative;display:grid;grid-template-columns:minmax(0,1fr) max-content;gap:6px;align-items:center;min-height:34px;padding:4px 6px 4px 15px;cursor:pointer}
.historyGroup.open .historyChild::before{content:'';position:absolute;left:calc(var(--history-guide-x) - var(--history-children-indent) + 1px);top:17px;width:var(--history-connector-length);height:1px;background:var(--history-guide-color);opacity:.72;pointer-events:none}
.historyGroup.open .historyChild:not(:last-child)::after{content:'';position:absolute;left:calc(var(--history-guide-x) - var(--history-children-indent));top:17px;bottom:-17px;width:1px;background:var(--history-guide-color);opacity:.72;pointer-events:none}
.historyChild.noDiff{cursor:default}
.historyTarget{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--vscode-foreground)}
.historyMeta{margin-top:2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--vscode-descriptionForeground);font-size:11px}
.historyActions{display:flex;align-items:center;gap:2px;white-space:nowrap}
.historyAction{height:22px;border:0;background:transparent;color:var(--vscode-icon-foreground);padding:0 5px;cursor:pointer;font:inherit}
.historyAction:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground))}
.historyAction:disabled{opacity:.45;cursor:default}
#gitPane{overflow:auto;padding:0;background:var(--vscode-sideBar-background)}
.gitRoot{display:flex;flex-direction:column;color:var(--vscode-foreground);font-size:12px}
.gitLoading{display:flex;align-items:center;gap:8px;padding:16px 12px;color:var(--vscode-descriptionForeground)}
.ghSpinner{width:13px;height:13px;flex:0 0 auto;border-radius:50%;border:1.6px solid var(--vscode-descriptionForeground);border-top-color:transparent;animation:ghspin .7s linear infinite}
@keyframes ghspin{to{transform:rotate(360deg)}}
.ghSvg{display:block;flex:0 0 auto}
.ghHead{display:flex;align-items:center;gap:8px;padding:10px 10px 7px}
.ghBranch{display:inline-flex;align-items:center;gap:5px;min-width:0;flex:1;padding:3px 9px;border-radius:11px;background:var(--vscode-badge-background);color:var(--vscode-badge-foreground);font-weight:600}
.ghBranch .ghSvg{opacity:.85}
.ghBranchName{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.ghSync{display:inline-flex;align-items:center;gap:9px;flex:0 0 auto;font-variant-numeric:tabular-nums}
.ghArrow{display:inline-flex;align-items:center;gap:2px}
.ghArrow.zero{opacity:.4}
.ghIconBtn{flex:0 0 auto;width:24px;height:24px;display:inline-flex;align-items:center;justify-content:center;border:0;border-radius:5px;background:transparent;color:var(--vscode-icon-foreground);cursor:pointer;padding:0}
.ghIconBtn:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground))}
.ghIconBtn:disabled{opacity:.5;cursor:default;background:transparent}
.ghIconBtn.spin .ghSvg{animation:ghspin .8s linear infinite}
.ghMeta{padding:0 12px 9px;color:var(--vscode-descriptionForeground);font-size:11px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.ghStatus{display:flex;align-items:center;gap:8px;margin:0 10px 10px;padding:7px 10px;border-radius:6px;background:var(--vscode-textBlockQuote-background,rgba(127,127,127,.09));border:1px solid var(--vscode-panel-border,transparent)}
.ghStatus .ghDot{width:8px;height:8px;border-radius:50%;flex:0 0 auto;background:currentColor}
.ghStatus.ok{color:var(--vscode-testing-iconPassed,var(--vscode-charts-green))}
.ghStatus.warn{color:var(--vscode-editorWarning-foreground,var(--vscode-charts-yellow))}
.ghStatus.err{color:var(--vscode-editorError-foreground,var(--vscode-charts-red))}
.ghStatusText{color:var(--vscode-foreground);min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.ghNote{margin:0 10px 10px;padding:6px 10px;border-radius:6px;background:var(--vscode-inputValidation-infoBackground,rgba(96,148,237,.1));border:1px solid var(--vscode-inputValidation-infoBorder,transparent);color:var(--vscode-foreground);font-size:11px;line-height:1.5;word-break:break-word}
.ghActions{display:flex;gap:6px;padding:0 10px 12px}
.ghPrimary{flex:1;min-height:30px;border:0;border-radius:6px;padding:0 10px;font:inherit;font-weight:600;cursor:pointer;color:var(--vscode-button-foreground);background:var(--vscode-button-background)}
.ghPrimary:hover{background:var(--vscode-button-hoverBackground,var(--vscode-button-background))}
.ghPrimary:disabled{opacity:.5;cursor:default}
.ghSecondary{flex:0 0 auto;display:inline-flex;align-items:center;gap:5px;min-height:30px;border:0;border-radius:6px;padding:0 12px;font:inherit;cursor:pointer;color:var(--vscode-button-secondaryForeground);background:var(--vscode-button-secondaryBackground)}
.ghSecondary:hover{background:var(--vscode-button-secondaryHoverBackground,var(--vscode-toolbar-hoverBackground))}
.ghSecondary:disabled{opacity:.5;cursor:default}
.ghSection{border-top:1px solid var(--vscode-sideBarSectionHeader-border,rgba(127,127,127,.15))}
.ghSectionHead{width:100%;display:flex;align-items:center;gap:6px;height:30px;padding:0 10px;border:0;background:transparent;color:var(--vscode-foreground);font:inherit;font-size:11px;font-weight:600;letter-spacing:.03em;text-transform:uppercase;cursor:pointer}
.ghSectionHead:hover{background:var(--vscode-list-hoverBackground)}
.ghSectionHead .ghSvg{color:var(--vscode-icon-foreground);transition:transform .12s ease}
.ghSectionHead.open .ghSvg{transform:rotate(90deg)}
.ghSectionTitle{flex:1;text-align:left;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.ghBadgeCount{min-width:18px;height:16px;padding:0 5px;border-radius:8px;display:inline-flex;align-items:center;justify-content:center;font-size:11px;font-weight:600;letter-spacing:0;color:var(--vscode-badge-foreground);background:var(--vscode-badge-background);opacity:.55}
.ghBadgeCount.has{opacity:1}
.ghChanges{padding:2px 0 6px}
.ghChange{display:flex;align-items:center;gap:8px;min-height:24px;padding:0 10px 0 14px;cursor:pointer}
.ghChange:hover{background:var(--vscode-list-hoverBackground)}
.ghChange:focus-visible{outline:1px solid var(--vscode-focusBorder);outline-offset:-1px}
.ghBadge{flex:0 0 auto;width:16px;text-align:center;font-family:var(--vscode-editor-font-family);font-size:11px;font-weight:600;color:var(--vscode-descriptionForeground)}
.ghBadge.added,.ghBadge.untracked{color:var(--vscode-gitDecoration-addedResourceForeground,var(--vscode-charts-green))}
.ghBadge.modified{color:var(--vscode-gitDecoration-modifiedResourceForeground,var(--vscode-charts-blue))}
.ghBadge.deleted{color:var(--vscode-gitDecoration-deletedResourceForeground,var(--vscode-charts-red))}
.ghBadge.conflict{color:var(--vscode-gitDecoration-conflictingResourceForeground,var(--vscode-editorWarning-foreground))}
.ghChangePath{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:12px}
.ghChangePath .ghDir{color:var(--vscode-descriptionForeground)}
.ghCommands{display:flex;flex-direction:column;gap:1px;padding:2px 8px 10px}
.gitCommand{display:flex;align-items:center;min-height:26px;border:0;border-radius:5px;padding:0 10px;font:inherit;cursor:pointer;background:transparent;color:var(--vscode-foreground);text-align:left}
.gitCommand:hover{background:var(--vscode-list-hoverBackground)}
.gitCommand:disabled{opacity:.45;cursor:default;background:transparent}
.ghEmpty{display:flex;align-items:center;gap:7px;padding:12px 14px;color:var(--vscode-descriptionForeground)}
.ghEmpty code,.ghNote code{font-family:var(--vscode-editor-font-family);font-size:11px;padding:1px 4px;border-radius:3px;background:var(--vscode-textCodeBlock-background,rgba(127,127,127,.15))}
.gitEmpty{padding:12px;color:var(--vscode-descriptionForeground)}
.ghSectionHead:focus-visible,.ghPrimary:focus-visible,.ghSecondary:focus-visible,.ghIconBtn:focus-visible,.gitCommand:focus-visible{outline:1px solid var(--vscode-focusBorder);outline-offset:-1px}
#rbsyncPane{flex:1;min-height:0;display:flex;flex-direction:column;position:relative}
#rbsyncBar{height:30px;display:flex;align-items:center;gap:6px;padding:4px;border-bottom:1px solid var(--vscode-sideBarSectionHeader-border,transparent)}
#rbsyncSearch{flex:1;min-width:0;height:22px;border:1px solid var(--vscode-input-border,transparent);background:var(--vscode-input-background);color:var(--vscode-input-foreground);padding:2px 5px;font:inherit}
#rbsyncOpen{flex:0 0 auto;width:24px;height:24px;display:flex;align-items:center;justify-content:center;border:0;background:transparent;color:var(--vscode-icon-foreground);padding:0;cursor:pointer;border-radius:4px}
#rbsyncOpen:hover{background:var(--vscode-toolbar-hoverBackground,var(--vscode-list-hoverBackground))}
#rbsyncOpen:active{background:var(--vscode-toolbar-activeBackground,var(--vscode-list-activeSelectionBackground))}
#rbsyncOpen svg{display:block}
#rbsyncBrowse{color:var(--vscode-textLink-foreground);cursor:pointer}
.rbsyncDim{color:var(--vscode-descriptionForeground);opacity:.8;font-size:11px}
#rbsyncTree{flex:1 1 auto;overflow:auto;padding:0;min-height:0;outline:none;position:relative}
.rbSizer{position:relative;width:100%}
.rbRows{position:absolute;left:0;right:0;top:0;will-change:transform}
.rbhi{background:var(--vscode-editor-findMatchHighlightBackground,rgba(234,140,0,.35));color:inherit;border-radius:2px}
.rbsyncHint{padding:14px;color:var(--vscode-descriptionForeground);line-height:1.6}
.rberr{padding:12px;color:var(--vscode-errorForeground);white-space:pre-wrap}
#rbsyncDrop{position:absolute;inset:0;display:none;align-items:center;justify-content:center;background:var(--vscode-list-dropBackground,rgba(0,120,215,.18));border:2px dashed var(--vscode-focusBorder);pointer-events:none;z-index:5}
#rbsyncDrop .rbsyncDropIn{font-weight:600;color:var(--vscode-foreground);background:var(--vscode-editor-background);padding:10px 16px;border-radius:8px;box-shadow:0 2px 10px rgba(0,0,0,.35)}
#rbsyncPane.rbdrag{outline:2px solid var(--vscode-focusBorder);outline-offset:-2px}
#rbsyncPane.rbdrag #rbsyncDrop{display:flex}
</style>
</head>
<body>
<div id="tabs"><button class="tabBtn active" data-tab="explorer">Explorer</button><button class="tabBtn" data-tab="history">History</button><button class="tabBtn" data-tab="git">Git</button><button class="tabBtn" data-tab="rbsync">Inspector</button></div>
<div id="explorerPane">
  <div id="bar"><input id="search" placeholder="Search" spellcheck="false"></div>
  <div id="suggestions">
    <div class="suggestTitle">Suggested Filters</div>
    <div class="suggestItem" data-insert="anchored="><span class="suggestIcon">A</span><span>anchored=</span></div>
    <div class="suggestItem" data-insert="locked="><span class="suggestIcon">L</span><span>locked=</span></div>
    <div class="suggestItem" data-insert="transparency="><span class="suggestIcon">%</span><span>transparency=</span></div>
    <div class="suggestItem" data-insert="material="><span class="suggestIcon">M</span><span>material=</span></div>
    <div class="suggestItem" data-insert="meshid="><span class="suggestIcon">#</span><span>meshid=</span></div>
    <div class="suggestItem" data-insert="textureid="><span class="suggestIcon">T</span><span>textureid=</span></div>
    <div class="suggestItem" data-insert="tag:"><span class="suggestIcon">&#9671;</span><span>tag:</span></div>
  </div>
  <div id="searchMeta"><span class="searchSummary">0 matches</span><span class="searchActions"><button class="iconBtn" id="prevMatch" title="Select previous match">&uarr;</button><button class="iconBtn" id="nextMatch" title="Select next match">&darr;</button><button class="iconBtn" id="selectMatches" title="Select all matches">&#9633;</button><button class="iconBtn" id="refreshResults" title="Refresh results">&#8635;</button></span></div>
  <div id="tree" tabindex="0">${initialRows || '<div id="treeEmpty">Loading services...</div>'}</div>
</div>
<div id="historyPane" class="hidden">
  <div id="historyHeader"><span id="historyTitle">Editor History</span><button class="iconBtn" id="refreshHistory" title="Refresh history">&#8635;</button></div>
  <div id="historyList"><div id="treeEmpty">Loading history...</div></div>
</div>
<div id="gitPane" class="hidden">
  <div id="gitApp"><div class="gitEmpty">Loading Git status...</div></div>
</div>
<div id="rbsyncPane" class="hidden">
  <div id="rbsyncBar"><input id="rbsyncSearch" placeholder="Search" spellcheck="false"><button id="rbsyncOpen" type="button" title="Open a .renium file" aria-label="Open a .renium file"><svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M1.75 2.5h3.9c.27 0 .53.1.72.3L7.5 4h6.25c.41 0 .75.34.75.75v8c0 .41-.34.75-.75.75H1.75A.75.75 0 0 1 1 12.75v-9.5c0-.41.34-.75.75-.75Zm.25 1.5v8h11V5.5H6.88L5.38 4H2Z"/></svg></button></div>
  <div id="rbsyncTree" tabindex="0"><div class="rbsyncHint">Open a <b>.renium</b> store with the folder button above, or <a id="rbsyncBrowse" href="#">browse for a file</a>.<br><span class="rbsyncDim">Legacy .rbsync stores are supported too.</span></div></div>
  <div id="rbsyncDrop"><div class="rbsyncDropIn">Drop to inspect</div></div>
</div>
<div id="menu" class="hidden"></div>
<div id="classPicker" class="hidden"><input id="classSearch" placeholder="ClassName" spellcheck="false"><div id="classList"></div></div>
<script>
(function(){
var vscode=acquireVsCodeApi(),ASSET=${JSON.stringify(assetBase)},CLASS_NAMES=${classNamesJson},AVAILABLE_ICONS=new Set(${assetIconNamesJson});
var nodes={},rootIds=[],expanded=new Set(),selectedId=null,lastHostSelectionId=null,referencePreviewId=null,filter='',menuNode=null,menuX=0,menuY=0;
var linkKeys={},externalPackageDrag=null,packageDragCursorSawDown=false;
function nodeLinkState(n){
  if(!n||!n.pathSegments||n.pathSegments.length<2)return null;
  return linkKeys[n.pathSegments[0]+String.fromCharCode(1)+n.pathSegments.slice(1).join('/')]||null;
}
function directReniumState(n){
  if(!n||n.kind==='service')return null;
  var linked=nodeLinkState(n),isPackage=n.hasPackageLink===true;
  if(linked==='broken')return{kind:'broken',inherited:false,package:isPackage};
  if(isPackage)return{kind:'package',inherited:false,package:true};
  if(linked==='linked')return{kind:'linked',inherited:false,package:false};
  return null;
}
function nodeReniumState(n){
  var direct=directReniumState(n);
  if(direct)return direct;
  var current=n;
  while(current&&current.parentId){
    current=nodes[current.parentId];
    var parentState=directReniumState(current);
    if(parentState)return{kind:parentState.kind,inherited:true,package:parentState.package};
  }
  return null;
}
function reniumBadgeHtml(state){
  if(!state)return '';
  var label=state.kind==='broken'?'Broken':(state.kind==='package'?'Package':'Linked');
  var title=state.inherited?label+' by parent Renium target':label+' Renium target';
  return '<span class="linkBadge '+esc(state.kind)+'" title="'+esc(title)+'">'+esc(label)+'</span>';
}
function canDesyncPackage(n){return !!n&&n.kind!=='service'&&(n.className==='PackageLink'||n.hasPackageLink===true)}
var renameId=null,renameOriginal='',suppressRenameFocusoutRender=false,renamePointerStartedInside=false,renameSuppressFocusoutUntil=0,draggedId=null,dropId=null,lastPointerRowId=null,screenOffsetX=null,screenOffsetY=null,addParentId=null,classActive=0,loadingIds={},loadDelayUntil={},autoLoadIds=[],matchIds=[],matchIndex=-1,searchLoading=false,searchRequested=false,searchLoaded=0,searchTotal=0,searchMatchCount=0,allMatchesSelected=false;
var searchPlanFilter=null,searchPlanGroups=[],selfMatchCache={},subtreeMatchCache={},searchExpanded=new Set(),renderFrame=0,pendingRenderAnchor=null;
var searchIndexDirty=true,searchEntries={},searchEntryIds=[],searchResultsFilter=null,searchVisibleSet=new Set(),searchResultIds=[];
var ROW_HEIGHT=22,VIRTUAL_OVERSCAN=40,flatRows=[],visibleRenderFrame=0,currentEmptyHtml='',totalRows=0,rowWindowStart=0,lastRequestedStart=-1,lastRequestedCount=0,lastRequestMode='normal',searchDebounce=null,rowRequestPending=false,searchPointerOpenUntil=0,searchRetainFocusUntil=0,searchRestoringFocus=false,searchSuggestionsShownThisFocus=false,rowCache={},rowCacheMode='normal',backendErrorRetryCount=0,searchRevision=0,searchInitialLoading=false,prefetchPending=false,prefetchTimer=null,lastScrollTop=0,lastScrollTime=0,scrollVelocityRows=0,scrollDirection=1;
var dragAutoScrollFrame=0,dragAutoScrollDirection=0,dragAutoScrollPointerY=0;
var tree=document.getElementById('tree'),search=document.getElementById('search'),searchMeta=document.getElementById('searchMeta'),suggestions=document.getElementById('suggestions'),menu=document.getElementById('menu');
var searchSummary=searchMeta.querySelector('.searchSummary'),prevMatch=document.getElementById('prevMatch'),nextMatch=document.getElementById('nextMatch'),selectMatches=document.getElementById('selectMatches'),refreshResults=document.getElementById('refreshResults');
var classPicker=document.getElementById('classPicker'),classSearch=document.getElementById('classSearch'),classList=document.getElementById('classList');
var tabs=document.getElementById('tabs'),explorerPane=document.getElementById('explorerPane'),historyPane=document.getElementById('historyPane'),gitPane=document.getElementById('gitPane'),gitApp=document.getElementById('gitApp'),historyList=document.getElementById('historyList'),historyTitle=document.getElementById('historyTitle'),refreshHistory=document.getElementById('refreshHistory');
var rbsyncPane=document.getElementById('rbsyncPane'),rbsyncTree=document.getElementById('rbsyncTree'),rbsyncSearch=document.getElementById('rbsyncSearch');
var saved=vscode.getState()||{};
var activeTab=saved.activeTab==='history'||saved.activeTab==='git'||saved.activeTab==='rbsync'?saved.activeTab:'explorer',historyGroups=[],historyLoading=false,historyLoaded=false,historyRestoring={},historyExpanded=new Set(Array.isArray(saved.historyExpanded)?saved.historyExpanded:[]);
var gitState=null,gitLoading=false,gitChangesOpen=saved.gitChangesOpen!==false,gitAdvancedOpen=!!saved.gitAdvancedOpen;
var hasClipboardInstance=false;
var packageDragDebugLast=0,packageDragDebugLastMessage='';
function debugPackageDrag(message){
  var text=String(message||'');
  var now=Date.now();
  var noisy=/^(row from|no row|target row|cursor)/.test(text);
  if(noisy&&text===packageDragDebugLastMessage&&now-packageDragDebugLast<500)return;
  if(noisy&&now-packageDragDebugLast<120)return;
  packageDragDebugLast=now;
  packageDragDebugLastMessage=text;
  vscode.postMessage({type:'packageDragDebug',message:text});
}
var VALUE_ICON_FALLBACKS={BinaryStringValue:1,Color3Value:1,DoubleConstrainedValue:1,IntConstrainedValue:1,IntValue:1,NumberValue:1,ObjectValue:1,StringValue:1,Vector3Value:1};
var CLASS_NAME_SET=new Set(CLASS_NAMES);
var FREQUENT_CLASS_DEFAULT=['Folder','Model','Part','Script','LocalScript','ModuleScript','Attachment','RemoteEvent','RemoteFunction','Configuration'];
var FREQUENT_CLASS_BY_SERVICE={
  Workspace:['Part','Model','Folder','SpawnLocation','Script','Attachment','WeldConstraint','PointLight','Sound','Highlight'],
  ReplicatedStorage:['Folder','ModuleScript','RemoteEvent','RemoteFunction','BindableEvent','BindableFunction','Configuration','Model','Part','Animation'],
  ReplicatedFirst:['LocalScript','ModuleScript','Folder','ScreenGui','Sound','Configuration','Model','Part','BindableEvent','Animation'],
  ServerScriptService:['Script','ModuleScript','Folder','Configuration','BindableEvent','BindableFunction','Model','Part','Sound','Animation'],
  ServerStorage:['Folder','ModuleScript','Script','Model','Part','Tool','Configuration','Animation','Sound','MeshPart'],
  StarterGui:['ScreenGui','Frame','TextLabel','TextButton','ImageLabel','ImageButton','ScrollingFrame','UIListLayout','UIPadding','UICorner'],
  StarterPack:['Tool','LocalScript','ModuleScript','Folder','Model','Part','Animation','Sound','Configuration','RemoteEvent'],
  StarterPlayer:['StarterPlayerScripts','StarterCharacterScripts','LocalScript','ModuleScript','Folder','Tool','Animation','Sound','Configuration','Model'],
  StarterPlayerScripts:['LocalScript','ModuleScript','Folder','Configuration','BindableEvent','BindableFunction','RemoteEvent','RemoteFunction','Sound','Animation'],
  StarterCharacterScripts:['LocalScript','Script','ModuleScript','Folder','Animation','Sound','Attachment','ParticleEmitter','Trail','Configuration'],
  Lighting:['Sky','Atmosphere','BloomEffect','ColorCorrectionEffect','SunRaysEffect','DepthOfFieldEffect','BlurEffect','Clouds','Folder','Script'],
  SoundService:['Sound','Folder','EqualizerSoundEffect','ReverbSoundEffect','CompressorSoundEffect','ChorusSoundEffect','DistortionSoundEffect','EchoSoundEffect','FlangeSoundEffect','Script'],
  MaterialService:['MaterialVariant','Folder','Configuration','ModuleScript','Script','StringValue','Model','Part','SurfaceAppearance','Texture'],
  Teams:['Team','Folder','Script','ModuleScript','Configuration','StringValue','BoolValue','Color3Value','Part','Model']
};
var FREQUENT_CLASS_BY_PARENT={
  Folder:['Folder','Model','Part','Script','LocalScript','ModuleScript','Attachment','Configuration','RemoteEvent','RemoteFunction'],
  Model:['Part','MeshPart','UnionOperation','Attachment','WeldConstraint','Motor6D','Script','LocalScript','ModuleScript','Folder'],
  Part:['Attachment','WeldConstraint','PointLight','ParticleEmitter','Decal','Texture','SurfaceGui','Highlight','Sound','Script'],
  MeshPart:['Attachment','WeldConstraint','PointLight','ParticleEmitter','SurfaceAppearance','Decal','Texture','Sound','Trail','Script'],
  Attachment:['ParticleEmitter','Trail','Beam','PointLight','SpotLight','Smoke','Fire','Sparkles','Sound','Script'],
  ScreenGui:['Frame','TextLabel','TextButton','ImageLabel','ImageButton','ScrollingFrame','UIListLayout','UIPadding','UICorner','UIStroke'],
  SurfaceGui:['Frame','TextLabel','TextButton','ImageLabel','ImageButton','ScrollingFrame','UIListLayout','UIPadding','UICorner','UIStroke'],
  BillboardGui:['Frame','TextLabel','TextButton','ImageLabel','ImageButton','UIListLayout','UIPadding','UICorner','UIStroke','UIScale'],
  Frame:['Frame','TextLabel','TextButton','ImageLabel','ImageButton','ScrollingFrame','UIListLayout','UIPadding','UICorner','UIStroke'],
  ScrollingFrame:['Frame','TextLabel','TextButton','ImageLabel','ImageButton','UIListLayout','UIPadding','UICorner','UIStroke','UISizeConstraint'],
  ViewportFrame:['Model','Part','Camera','Folder','Attachment','PointLight','Highlight','Script','ModuleScript','Sound'],
  TextButton:['UICorner','UIStroke','UIGradient','UIScale','UITextSizeConstraint','LocalScript','Sound','Frame','ImageLabel','UIAspectRatioConstraint'],
  ImageButton:['UICorner','UIStroke','UIGradient','UIScale','UIAspectRatioConstraint','LocalScript','Sound','Frame','ImageLabel','TextLabel'],
  Tool:['Part','LocalScript','Script','ModuleScript','Animation','Sound','Attachment','Folder','Configuration','Handle']
};
if(Array.isArray(saved.expanded))expanded=new Set(saved.expanded);
if(saved.selectedId)selectedId=saved.selectedId;
function save(){vscode.setState({expanded:Array.from(expanded),selectedId:selectedId,activeTab:activeTab,historyExpanded:Array.from(historyExpanded),gitChangesOpen:gitChangesOpen,gitAdvancedOpen:gitAdvancedOpen,screenOffsetX:screenOffsetX,screenOffsetY:screenOffsetY,screenOffsetWX:screenOffsetWX,screenOffsetWY:screenOffsetWY})}
function syncSelectionToHost(){
  if(selectedId&&nodes[selectedId]&&selectedId!==lastHostSelectionId){
    lastHostSelectionId=selectedId;
    vscode.postMessage({type:'selectNode',nodeId:selectedId});
  }
}
function expandAncestors(id){
  var n=nodes[id],changed=false;
  while(n&&n.parentId){
    if(!expanded.has(n.parentId)){expanded.add(n.parentId);changed=true}
    n=nodes[n.parentId];
  }
  if(changed)save();
}
function esc(s){return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;')}
function canShowSearchSuggestions(){
  return activeTab==='explorer'&&!search.value.trim();
}
function showSearchSuggestions(){
  if(canShowSearchSuggestions())suggestions.classList.add('active');
}
function showSearchSuggestionsOnce(){
  if(searchSuggestionsShownThisFocus)return;
  if(canShowSearchSuggestions()){
    suggestions.classList.add('active');
    searchSuggestionsShownThisFocus=true;
  }
}
function hideSearchSuggestions(){
  suggestions.classList.remove('active');
}
function setActiveTab(tab, skipLoad){
  activeTab=tab==='history'||tab==='git'||tab==='rbsync'?tab:'explorer';
  save();
  Array.prototype.forEach.call(tabs.querySelectorAll('.tabBtn'),function(btn){
    btn.classList.toggle('active',btn.dataset.tab===activeTab);
  });
  explorerPane.classList.toggle('hidden',activeTab!=='explorer');
  historyPane.classList.toggle('hidden',activeTab!=='history');
  gitPane.classList.toggle('hidden',activeTab!=='git');
  rbsyncPane.classList.toggle('hidden',activeTab!=='rbsync');
  hideSearchSuggestions();
  closeMenus();
  if(activeTab==='history'){
    renderHistory();
    if(!skipLoad&&!historyLoaded&&!historyLoading)loadHistory();
  }else if(activeTab==='git'){
    renderGit();
    if(!skipLoad)vscode.postMessage({type:'gitReady'});
  }else if(activeTab==='rbsync'){
    if(typeof rbPaint==='function')rbPaint();
  }else if(activeTab==='explorer'&&!skipLoad){
    requestRows(false);
  }
}
function prepareReferencePreview(){
  if(activeTab!=='explorer')setActiveTab('explorer',true);
  if(filter||search.value.trim()){
    search.value='';
    searchRevision++;
    filter='';
    if(searchDebounce){clearTimeout(searchDebounce);searchDebounce=null}
    if(prefetchTimer){clearTimeout(prefetchTimer);prefetchTimer=null}
    prefetchPending=false;
    searchInitialLoading=false;
    searchExpanded.clear();
    searchRequested=false;
    searchLoading=false;
    searchLoaded=0;
    searchTotal=0;
    searchMatchCount=0;
    matchIds=[];
    allMatchesSelected=false;
    rowWindowStart=0;
    totalRows=0;
    flatRows=[];
    lastRequestedStart=-1;
    lastRequestMode='normal';
    resetRowCache('normal');
    currentEmptyHtml='<div id="treeEmpty">Loading...</div>';
    renderFlatRows();
    updateSearchMeta();
  }
}
function loadHistory(){
  historyLoading=true;
  renderHistory();
  vscode.postMessage({type:'loadHistory'});
}
function historyMetaText(entry){
  var bits=[];
  if(entry.service)bits.push(entry.service);
  if(entry.className)bits.push(entry.className);
  if(entry.settingsId)bits.push(entry.settingsId);
  if(entry.timeLabel||entry.createdLabel)bits.push(entry.timeLabel||entry.createdLabel);
  return bits.join(' · ');
}
function renderHistory(){
  if(!historyList)return;
  if(historyLoading&&historyGroups.length===0){
    historyTitle.textContent='Editor History';
    historyList.innerHTML='<div id="treeEmpty">Loading history...</div>';
    return;
  }
  var editTotal=0;
  for(var gCount=0;gCount<historyGroups.length;gCount++)editTotal+=historyGroups[gCount].entryCount||0;
  historyTitle.textContent=historyGroups.length?('Editor History ('+historyGroups.length+' sessions, '+editTotal+' edits)'):'Editor History';
  if(historyGroups.length===0){
    historyList.innerHTML='<div id="treeEmpty">No editor history found.</div>';
    return;
  }
  var html='';
  for(var i=0;i<historyGroups.length;i++){
    var group=historyGroups[i],open=historyExpanded.has(group.id),restoring=!!historyRestoring[group.id];
    var primary=(group.items||[])[0];
    var restoreIds=(group.items||[]).map(function(item){return item.restoreId}).filter(Boolean);
    html+='<div class="historyGroup'+(open?' open':'')+'" data-group-id="'+esc(group.id)+'">';
    html+='<div class="historyGroupHeader" data-action="toggleHistoryGroup" data-group-id="'+esc(group.id)+'">';
    html+='<button class="historyTwisty" data-action="toggleHistoryGroup" data-group-id="'+esc(group.id)+'" title="'+(open?'Collapse':'Expand')+'"></button>';
    html+='<div class="historyMain"><div class="historyTarget" title="'+esc(group.title||'')+'">'+esc(group.title||'History session')+'</div>';
    html+='<div class="historyMeta">'+esc(group.subtitle||'')+'</div></div>';
    html+='<div class="historyActions">';
    if(primary&&primary.hasSourceBackup){
      html+='<button class="historyAction" data-action="compareHistoryBackup" data-id="'+esc(primary.openId||primary.id)+'" title="Compare backup with current file">Diff</button>';
      html+='<button class="historyAction" data-action="openHistoryBackup" data-id="'+esc(primary.openId||primary.id)+'" title="Open source backup">Open</button>';
    }
    html+='<button class="historyAction" data-action="restoreHistoryGroup" data-group-id="'+esc(group.id)+'" data-ids="'+esc(JSON.stringify(restoreIds))+'" '+(restoring?'disabled':'')+' title="Restore this edit session">'+(restoring?'Restoring':'Restore')+'</button>';
    html+='</div></div>';
    if(open){
      html+='<div class="historyChildren">';
      var items=group.items||[];
      for(var j=0;j<items.length;j++){
        var entry=items[j],entryRestoring=!!historyRestoring[entry.restoreId];
        html+='<div class="historyChild'+(entry.hasSourceBackup?'':' noDiff')+'" data-id="'+esc(entry.restoreId)+'" data-open-id="'+esc(entry.openId||entry.id)+'" title="'+(entry.hasSourceBackup?'Click to compare with current file':'')+'">';
        html+='<div class="historyChildMain"><div class="historyTarget" title="'+esc(entry.targetLabel||'')+'">'+esc(entry.targetLabel||entry.service||'History entry')+'</div>';
        html+='<div class="historyMeta">'+esc(historyMetaText(entry)+(entry.editCount>1?' · '+entry.editCount+' versions':''))+'</div></div>';
        html+='<div class="historyActions">';
        html+='<button class="historyAction" data-action="compareHistoryBackup" data-id="'+esc(entry.openId||entry.id)+'" '+(entry.hasSourceBackup?'':'disabled')+' title="Compare backup with current file">Diff</button>';
        html+='<button class="historyAction" data-action="openHistoryBackup" data-id="'+esc(entry.openId||entry.id)+'" '+(entry.hasSourceBackup?'':'disabled')+' title="Open source backup">Open</button>';
        html+='<button class="historyAction" data-action="restoreHistory" data-id="'+esc(entry.restoreId||entry.id)+'" '+(entryRestoring?'disabled':'')+' title="Restore this item">'+(entryRestoring?'Restoring':'Restore')+'</button>';
        html+='</div></div>';
      }
      html+='</div>';
    }
    html+='</div>';
  }
  historyList.innerHTML=html;
}
function gitDisabled(enabled){return enabled&&!gitLoading?'':' disabled'}
function gitEntryBadge(entry){
  if(entry.conflicted)return '!';
  if(entry.deleted)return 'D';
  if(entry.untracked)return 'U';
  if(entry.kind==='added')return 'A';
  if(entry.kind==='renamed')return 'R';
  if(entry.kind==='copied')return 'C';
  if(entry.kind==='typechange')return 'T';
  return 'M';
}
function gitEntryClass(entry){
  if(entry.conflicted)return 'conflict';
  if(entry.deleted)return 'deleted';
  if(entry.untracked)return 'untracked';
  if(entry.kind==='added'||entry.kind==='copied')return 'added';
  return 'modified';
}
function gitActionButton(label, action, enabled, title){
  return '<button class="gitCommand" data-gh-action="'+esc(action)+'" title="'+esc(title||label)+'"'+gitDisabled(enabled)+'>'+esc(label)+'</button>';
}
function toggleGitGroup(name){
  if(name==='changes')gitChangesOpen=!gitChangesOpen;
  else if(name==='actions')gitAdvancedOpen=!gitAdvancedOpen;
  save();
  renderGit();
}
function closeGitActions(){
  if(!gitAdvancedOpen)return;
  gitAdvancedOpen=false;
  save();
  renderGit();
}
function ghSvg(inner,cls){return '<svg class="ghSvg'+(cls?' '+cls:'')+'" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.35" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">'+inner+'</svg>'}
function ghBranchIcon(){return ghSvg('<circle cx="4.5" cy="4" r="1.6"/><circle cx="4.5" cy="12" r="1.6"/><circle cx="11.5" cy="5.5" r="1.6"/><path d="M4.5 5.6v4.8"/><path d="M11.5 7.1c0 2.3-2.5 2.7-4.1 3.1"/>')}
function ghUpIcon(){return ghSvg('<path d="M8 12.5V4"/><path d="M4.8 7.2 8 4l3.2 3.2"/>')}
function ghDownIcon(){return ghSvg('<path d="M8 3.5V12"/><path d="M4.8 8.8 8 12l3.2-3.2"/>')}
function ghRefreshIcon(){return ghSvg('<path d="M12.8 8a4.8 4.8 0 1 1-1.4-3.4"/><path d="M12.9 2.7v2.6h-2.6"/>')}
function ghCheckIcon(){return ghSvg('<path d="M3.4 8.4 6.3 11.3 12.6 4.6"/>')}
function ghTwisty(){return ghSvg('<path d="M6 4l4 4-4 4"/>')}
function renderGit(){
  if(!gitApp)return;
  if(!gitState){
    gitApp.innerHTML='<div class="gitLoading"><span class="ghSpinner"></span>Loading Git status...</div>';
    return;
  }
  var counts=gitState.counts||{},entries=Array.isArray(gitState.entries)?gitState.entries:[];
  var conflicts=counts.conflicted||0,behind=gitState.behind||0,ahead=gitState.ahead||0,total=counts.total||0;
  var branch=gitState.branch||'unknown';
  var remote=gitState.remote||'origin';
  var trusted=gitState.trusted!==false;
  var connected=!!gitState.connected;
  var canRepo=trusted&&connected;
  var canSync=canRepo&&conflicts===0;
  var canSetup=trusted;
  var dotClass=!trusted||!gitState.ok||conflicts?'err':(!connected||behind)?'warn':'ok';
  var statusText=!trusted?'Workspace not trusted':!connected?'No remote connected':conflicts?(conflicts+' conflict'+(conflicts===1?'':'s')+' to resolve'):behind?(behind+' commit'+(behind===1?'':'s')+' to pull'):ahead?(ahead+' commit'+(ahead===1?'':'s')+' to push'):'Up to date';
  var repoMeta=connected?((gitState.upstream||remote)+(gitState.remoteUrl?' · '+gitState.remoteUrl:'')):'Not connected to a remote';
  var message=gitState.message||'';
  var primaryActionLabel=gitLoading?'Working...':connected?'Commit & Push':'Connect Remote...';
  var primaryAction=connected?'commitPush':'connect';
  var primaryActionEnabled=connected?canSync:canSetup;
  var html='<div class="gitRoot">';
  html+='<div class="ghHead">';
  html+='<span class="ghBranch" title="Current branch">'+ghBranchIcon()+'<span class="ghBranchName">'+esc(branch)+'</span></span>';
  html+='<span class="ghSync">';
  html+='<span class="ghArrow'+(ahead?'':' zero')+'" title="'+esc(ahead)+' to push">'+ghUpIcon()+esc(ahead)+'</span>';
  html+='<span class="ghArrow'+(behind?'':' zero')+'" title="'+esc(behind)+' to pull">'+ghDownIcon()+esc(behind)+'</span>';
  html+='</span>';
  html+='<button class="ghIconBtn'+(gitLoading?' spin':'')+'" data-gh-refresh="1" title="Refresh Git status"'+(gitLoading?' disabled':'')+'>'+ghRefreshIcon()+'</button>';
  html+='</div>';
  html+='<div class="ghMeta" title="'+esc(repoMeta)+'">'+esc(repoMeta)+'</div>';
  html+='<div class="ghStatus '+dotClass+'"><span class="ghDot"></span><span class="ghStatusText">'+esc(gitLoading?'Syncing...':statusText)+'</span></div>';
  if(message)html+='<div class="ghNote">'+esc(message)+'</div>';
  html+='<div class="ghActions">';
  html+='<button class="ghPrimary" data-gh-action="'+esc(primaryAction)+'"'+gitDisabled(primaryActionEnabled)+'>'+esc(primaryActionLabel)+'</button>';
  if(connected)html+='<button class="ghSecondary" data-gh-action="pull"'+gitDisabled(canSync)+' title="Pull from '+esc(remote)+'">'+ghDownIcon()+'Pull'+(behind?' '+esc(behind):'')+'</button>';
  else html+='<button class="ghSecondary" data-gh-output="1"'+gitDisabled(trusted)+'>Show Output</button>';
  html+='</div>';
  html+='<div class="ghSection">';
  html+='<button class="ghSectionHead '+(gitChangesOpen?'open':'')+'" data-gh-group="changes" aria-expanded="'+(gitChangesOpen?'true':'false')+'">'+ghTwisty()+'<span class="ghSectionTitle">Changes</span><span class="ghBadgeCount'+(total?' has':'')+'">'+esc(total)+'</span></button>';
  if(gitChangesOpen){
    html+='<div class="ghChanges">';
    if(entries.length===0){
      html+='<div class="ghEmpty">'+ghCheckIcon()+'<span>No changes in <code>src/</code></span></div>';
    }else{
      for(var i=0;i<Math.min(entries.length,200);i++){
        var entry=entries[i];
        var badge=gitEntryBadge(entry),badgeClass=gitEntryClass(entry);
        var path=String(entry.path||''),slash=path.lastIndexOf('/');
        var pathHtml=slash>=0?'<span class="ghDir">'+esc(path.slice(0,slash+1))+'</span>'+esc(path.slice(slash+1)):esc(path);
        html+='<div class="ghChange" data-gh-diff="'+esc(path)+'" role="button" tabindex="0" title="Open diff: '+esc(path)+'"><span class="ghBadge '+badgeClass+'">'+esc(badge)+'</span><span class="ghChangePath">'+pathHtml+'</span></div>';
      }
      if(entries.length>200)html+='<div class="ghNote">Showing first 200 of '+esc(entries.length)+' changes.</div>';
    }
    html+='</div>';
  }
  html+='</div>';
  html+='<div class="ghSection">';
  html+='<button class="ghSectionHead '+(gitAdvancedOpen?'open':'')+'" data-gh-group="actions" aria-expanded="'+(gitAdvancedOpen?'true':'false')+'">'+ghTwisty()+'<span class="ghSectionTitle">Repository Actions</span></button>';
  if(gitAdvancedOpen){
    html+='<div class="ghCommands">';
    html+=gitActionButton('Full Sync, Commit & Push','syncCommitPush',canSync,'Run Renium Full Sync before committing src changes');
    html+=gitActionButton('Fetch','fetch',canRepo,'Fetch remote refs');
    html+=gitActionButton('Connect Remote...','connect',canSetup,'Initialize or configure the Git remote');
    html+=gitActionButton('Open on Git','openRemote',canRepo,'Open the configured remote in a browser');
    html+=gitActionButton('Checkout Branch...','checkoutBranch',canSync,'Switch to another local branch');
    html+=gitActionButton('Create Branch...','createBranch',canSync,'Create a new branch');
    html+=gitActionButton('Publish Branch','publishBranch',canSync,'Publish the current branch upstream');
    html+=gitActionButton('Log Status','status',trusted,'Write detailed Git status to the Renium output');
    html+='<button class="gitCommand" data-gh-output="1"'+gitDisabled(trusted)+'>Show Output</button>';
    html+='</div>';
  }
  html+='</div></div>';
  gitApp.innerHTML=html;
}
function iconName(className){
  var preferred=VALUE_ICON_FALLBACKS[className]?'Value':className;
  if(AVAILABLE_ICONS.has(preferred))return preferred;
  var fallback=className&&className.endsWith('Service')?'Service':'Class';
  return AVAILABLE_ICONS.has(fallback)?fallback:preferred;
}
function closeMenus(){menu.classList.add('hidden');classPicker.classList.add('hidden');hideSearchSuggestions()}
function uniqueClassNames(items){
  var out=[],seen={};
  for(var i=0;i<items.length;i++){
    var name=String(items[i]||'');
    if(!CLASS_NAME_SET.has(name)||seen[name])continue;
    seen[name]=1;
    out.push(name);
  }
  return out;
}
function frequentClassesForNodeId(id){
  var node=id&&nodes[id],preferred=[];
  if(node){
    var serviceClasses=FREQUENT_CLASS_BY_SERVICE[node.service||''];
    var parentClasses=FREQUENT_CLASS_BY_PARENT[node.className||''];
    if(Array.isArray(serviceClasses))preferred=preferred.concat(serviceClasses);
    if(Array.isArray(parentClasses))preferred=preferred.concat(parentClasses);
  }
  preferred=uniqueClassNames(preferred.concat(FREQUENT_CLASS_DEFAULT));
  return preferred.slice(0,10);
}
function orderedClassNamesForParent(id){
  var preferred=frequentClassesForNodeId(id),out=[],seen={};
  for(var i=0;i<preferred.length;i++){
    var preferredName=preferred[i];
    seen[preferredName]=1;
    out.push(preferredName);
  }
  for(var j=0;j<CLASS_NAMES.length;j++){
    var className=CLASS_NAMES[j];
    if(!seen[className])out.push(className);
  }
  return out;
}
function rowEl(id){
  var rows=tree.querySelectorAll('.row');
  for(var i=0;i<rows.length;i++)if(rows[i].dataset.id===id)return rows[i];
  return null;
}
function flatRowIndex(id){
  for(var i=0;i<flatRows.length;i++)if(flatRows[i].id===id)return rowWindowStart+i;
  return -1;
}
function firstVisibleRow(){
  var treeRect=tree.getBoundingClientRect(),rows=tree.querySelectorAll('.row[data-id]');
  for(var i=0;i<rows.length;i++){
    var rect=rows[i].getBoundingClientRect();
    if(rect.bottom>=treeRect.top&&rect.top<=treeRect.bottom)return rows[i];
  }
  return null;
}
function captureScrollAnchor(id){
  if(id){
    var flatIndex=flatRowIndex(id);
    if(flatIndex>=0)return {id:id,top:flatIndex*ROW_HEIGHT-tree.scrollTop,scrollTop:tree.scrollTop};
  }
  var row=id?rowEl(id):firstVisibleRow();
  if(!row)return {scrollTop:tree.scrollTop};
  return {id:row.dataset.id,top:row.getBoundingClientRect().top-tree.getBoundingClientRect().top,scrollTop:tree.scrollTop};
}
function restoreScrollAnchor(anchor){
  if(!anchor)return;
  if(anchor.id){
    var flatIndex=flatRowIndex(anchor.id);
    if(flatIndex>=0){
      tree.scrollTop=Math.max(0,flatIndex*ROW_HEIGHT-anchor.top);
      return;
    }
  }
  var row=anchor.id?rowEl(anchor.id):null;
  if(row){
    tree.scrollTop+=row.getBoundingClientRect().top-tree.getBoundingClientRect().top-anchor.top;
  }else if(typeof anchor.scrollTop==='number'){
    tree.scrollTop=anchor.scrollTop;
  }
}
function visibleStart(){
  return Math.max(0,Math.floor(tree.scrollTop/ROW_HEIGHT)-VIRTUAL_OVERSCAN);
}
function visibleCount(){
  return Math.max(40,Math.ceil((tree.clientHeight||300)/ROW_HEIGHT)+VIRTUAL_OVERSCAN*2);
}
function resetRowCache(mode){
  rowCache={};
  rowCacheMode=mode||((filter?'search':'normal'));
}
function rememberRows(rows){
  for(var i=0;i<rows.length;i++){
    var row=rows[i];
    if(row&&row.id){
      nodes[row.id]=row;
      delete loadingIds[row.id];
    }
  }
}
function pruneRowCache(start,count){
  var span=rowCacheMode==='search'?30000:10000;
  var keepBefore=Math.max(0,start-span),keepAfter=start+count+span;
  Object.keys(rowCache).forEach(function(key){
    var index=Number(key);
    if(index<keepBefore||index>keepAfter){
      var row=rowCache[key];
      if(row&&row.id&&row.id!==selectedId)delete nodes[row.id];
      delete rowCache[key];
    }
  });
}
function cachedWindow(start,count){
  var rows=[],limit=totalRows>0?Math.min(count,Math.max(0,totalRows-start)):count;
  for(var i=0;i<limit;i++){
    rows.push(rowCache[start+i]||{type:'loading',depth:0});
  }
  return rows;
}
function optimisticDelete(id){
  if(!id)return;
  var anchor=captureScrollAnchor(),idx=-1,row=null;
  for(var i=0;i<flatRows.length;i++){
    if(flatRows[i]&&flatRows[i].id===id){idx=i;row=flatRows[i];break}
  }
  delete nodes[id];delete loadingIds[id];expanded.delete(id);
  if(selectedId===id)selectedId=null;
  if(idx>=0&&row){
    var depth=Number(row.depth)||0,removeCount=1;
    while(idx+removeCount<flatRows.length){
      var next=flatRows[idx+removeCount];
      if(!next||typeof next.depth!=='number'||next.depth<=depth)break;
      if(next.id){delete nodes[next.id];delete loadingIds[next.id];expanded.delete(next.id);if(selectedId===next.id)selectedId=null}
      removeCount++;
    }
    flatRows.splice(idx,removeCount);
    totalRows=Math.max(0,totalRows-removeCount);
  }
  rowCache={};
  save();render(anchor);syncSelectionToHost();
}
function firstMissingRow(start,end){
  start=Math.max(0,start);end=Math.min(totalRows,end);
  for(var i=start;i<end;i++)if(!rowCache[i])return i;
  return -1;
}
function lastMissingRow(start,end){
  start=Math.max(0,start);end=Math.min(totalRows,end);
  for(var i=end-1;i>=start;i--)if(!rowCache[i])return i;
  return -1;
}
function updateScrollVelocity(){
  var now=(typeof performance!=='undefined'&&performance.now)?performance.now():Date.now();
  var top=tree.scrollTop||0;
  if(!lastScrollTime){
    lastScrollTime=now;lastScrollTop=top;return;
  }
  var dt=Math.max(16,now-lastScrollTime),dy=top-lastScrollTop;
  if(dy!==0){
    var instant=(dy/ROW_HEIGHT)/(dt/1000);
    scrollVelocityRows=scrollVelocityRows*0.55+instant*0.45;
    scrollDirection=dy<0?-1:1;
  }else{
    scrollVelocityRows*=0.82;
  }
  lastScrollTime=now;lastScrollTop=top;
}
function schedulePrefetch(){
  if(prefetchTimer)clearTimeout(prefetchTimer);
  prefetchTimer=setTimeout(function(){
    prefetchTimer=null;
    if(prefetchPending||rowRequestPending||totalRows<=0)return;
    var mode=filter?'search':'normal',chunk=mode==='search'?1800:700;
    if(mode==='search'&&searchInitialLoading)return;
    if(rowCacheMode!==mode)return;
    var currentStart=visibleStart(),currentEnd=currentStart+visibleCount();
    var speedRows=Math.abs(scrollVelocityRows);
    var lookAhead=Math.max(chunk,Math.min(chunk*6,visibleCount()+Math.ceil(speedRows*0.85)));
    var direction=scrollDirection||1,missing=-1;
    if(direction>=0){
      missing=firstMissingRow(currentStart,Math.min(totalRows,currentEnd+lookAhead));
      if(missing<0)missing=lastMissingRow(Math.max(0,currentStart-Math.floor(lookAhead*0.35)),currentEnd);
    }else{
      missing=lastMissingRow(Math.max(0,currentStart-lookAhead),currentEnd);
      if(missing<0)missing=firstMissingRow(currentStart,Math.min(totalRows,currentEnd+Math.floor(lookAhead*0.35)));
    }
    if(missing<0)return;
    var start=Math.max(0,Math.floor(missing/chunk)*chunk);
    prefetchPending=true;
    vscode.postMessage({type:'prefetchRows',start:start,count:chunk,mode:mode,revision:searchRevision});
  },0);
}
function requestRows(force){
  var start=visibleStart(),count=visibleCount(),mode=filter?'search':'normal';
  if(mode==='search'&&searchInitialLoading&&totalRows===0)return;
  var missingVisible=totalRows>0&&firstMissingRow(start,start+count)>=0;
  if(!force&&!missingVisible&&start===lastRequestedStart&&count===lastRequestedCount&&mode===lastRequestMode)return;
  if(rowCacheMode!==mode)resetRowCache(mode);
  lastRequestedStart=start;lastRequestedCount=count;lastRequestMode=mode;
  rowRequestPending=true;
  if(totalRows>0){
    rowWindowStart=start;
    flatRows=cachedWindow(start,count);
    renderFlatRows();
  }
  var speedRows=Math.abs(scrollVelocityRows);
  var maxRequest=mode==='search'?2600:1400;
  var requestCount=Math.max(count,Math.min(maxRequest,count+Math.ceil(speedRows*1.2)+(mode==='search'?900:400)));
  vscode.postMessage({type:'getRows',start:start,count:requestCount,mode:mode,revision:searchRevision});
}
function canDrag(n){return !!n&&n.kind!=='service'&&n.canMove!==false}
function isDescendant(id,ancestorId){
  var n=nodes[id];
  while(n&&n.parentId){if(n.parentId===ancestorId)return true;n=nodes[n.parentId]}
  return false;
}
function canDrop(dragId,targetId){
  var drag=nodes[dragId],target=nodes[targetId];
  return canDrag(drag)&&!!target&&dragId!==targetId&&!isDescendant(targetId,dragId);
}
function clearDropTarget(){
  if(dropId){
    var old=rowEl(dropId);
    if(old)old.classList.remove('drop-target');
  }
  dropId=null;
}
function markDropTarget(id){
  if(!id)return;
  if(dropId!==id){
    clearDropTarget();
    dropId=id;
  }
  var row=rowEl(id);
  if(externalPackageDrag)debugPackageDrag('target row '+id+' rendered='+(row?'1':'0')+' windowStart='+rowWindowStart+' flatRows='+flatRows.length+' totalRows='+totalRows);
  if(row)row.classList.add('drop-target');
}
function rememberPointerRow(row){
  if(!row||!row.dataset||!row.dataset.id||!nodes[row.dataset.id])return null;
  lastPointerRowId=row.dataset.id;
  return lastPointerRowId;
}
function packageDropTargetFromState(preferPointer){
  if(dropId&&nodes[dropId])return dropId;
  if(preferPointer&&lastPointerRowId&&nodes[lastPointerRowId])return lastPointerRowId;
  if(selectedId&&nodes[selectedId])return selectedId;
  if(lastPointerRowId&&nodes[lastPointerRowId])return lastPointerRowId;
  return null;
}
function markPackageFallbackTarget(reason){
  var targetId=packageDropTargetFromState();
  if(targetId){
    if(externalPackageDrag)debugPackageDrag(reason+': fallback target '+targetId);
    markDropTarget(targetId);
  }
  else clearDropTarget();
  return targetId;
}
var screenOffsetWX=null,screenOffsetWY=null;
if(typeof saved.screenOffsetX==='number'&&typeof saved.screenOffsetY==='number'){
  screenOffsetX=saved.screenOffsetX;screenOffsetY=saved.screenOffsetY;
  screenOffsetWX=typeof saved.screenOffsetWX==='number'?saved.screenOffsetWX:null;
  screenOffsetWY=typeof saved.screenOffsetWY==='number'?saved.screenOffsetWY:null;
}
function rememberPointerEvent(e){
  if(!e)return;
  if(typeof e.screenX==='number'&&typeof e.clientX==='number'){screenOffsetX=e.screenX-e.clientX;screenOffsetWX=typeof window.screenX==='number'?window.screenX:null;}
  if(typeof e.screenY==='number'&&typeof e.clientY==='number'){screenOffsetY=e.screenY-e.clientY;screenOffsetWY=typeof window.screenY==='number'?window.screenY:null;}
}
document.addEventListener('pointermove',rememberPointerEvent,{passive:true,capture:true});
function rowFromClientPoint(clientX,clientY,source){
  var x=Number(clientX)||0,y=Number(clientY)||0;
  var el=document.elementFromPoint(x,y);
  var row=el&&el.closest?el.closest('.row'):null;
  if(row&&tree.contains(row)){
    rememberPointerRow(row);
    if(externalPackageDrag)debugPackageDrag('row from '+source+' elementFromPoint '+row.dataset.id);
    return row;
  }
  var rect=tree.getBoundingClientRect();
  if(y<rect.top||y>rect.bottom){
    if(externalPackageDrag)debugPackageDrag('no row from '+source+': y outside tree y='+Math.round(y)+' top='+Math.round(rect.top)+' bottom='+Math.round(rect.bottom));
    return null;
  }
  var rowIndex=Math.floor((tree.scrollTop+y-rect.top)/ROW_HEIGHT);
  var localIndex=rowIndex-rowWindowStart;
  var item=localIndex>=0&&localIndex<flatRows.length?flatRows[localIndex]:null;
  if(!item||item.type!=='node'||!item.id){
    if(externalPackageDrag)debugPackageDrag('no row from '+source+': rowIndex='+rowIndex+' local='+localIndex+' windowStart='+rowWindowStart+' flatRows='+flatRows.length+' total='+totalRows);
    return null;
  }
  lastPointerRowId=item.id;
  if(externalPackageDrag)debugPackageDrag('row from '+source+' coordinates '+item.id+' rowIndex='+rowIndex);
  return rowEl(item.id);
}
function dragEventRow(e){
  rememberPointerEvent(e);
  return rowFromClientPoint(e.clientX||0,e.clientY||0,'event');
}
function screenCursorRow(screenX,screenY,bounds){
  var x=Number(screenX),y=Number(screenY),candidates=[];
  if(!isFinite(x)||!isFinite(y)){
    debugPackageDrag('cursor invalid screen='+screenX+','+screenY);
    return null;
  }
  var wx=typeof window.screenX==='number'?window.screenX:(typeof window.screenLeft==='number'?window.screenLeft:0);
  var wy=typeof window.screenY==='number'?window.screenY:(typeof window.screenTop==='number'?window.screenTop:0);
  if(typeof screenOffsetX==='number'&&typeof screenOffsetY==='number'){
    var adjX=screenOffsetX+(typeof screenOffsetWX==='number'?wx-screenOffsetWX:0);
    var adjY=screenOffsetY+(typeof screenOffsetWY==='number'?wy-screenOffsetWY:0);
    candidates.push({source:'cursor calibrated',x:x-adjX,y:y-adjY});
  }
  if(bounds){
    var left=Number(bounds.left),top=Number(bounds.top),right=Number(bounds.right),bottom=Number(bounds.bottom);
    var width=right-left,height=bottom-top;
    if(isFinite(left)&&isFinite(top)&&width>0&&height>0&&x>=left&&x<=right&&y>=top&&y<=bottom){
      var scaleX=width/(window.innerWidth||width);
      var scaleY=height/(window.innerHeight||height);
      if(scaleX>=0.5&&scaleX<=4&&scaleY>=0.5&&scaleY<=4){
        candidates.push({source:'cursor hwnd',x:(x-left)/(scaleX||1),y:(y-top)/(scaleY||1)});
      }
      else debugPackageDrag('cursor hwnd rejected scale='+scaleX.toFixed(2)+','+scaleY.toFixed(2)+' hwnd='+left+','+top+','+right+','+bottom+' inner='+window.innerWidth+','+window.innerHeight);
    }
    else debugPackageDrag('cursor outside hwnd screen='+x+','+y+' hwnd='+left+','+top+','+right+','+bottom);
  }
  candidates.push({source:'cursor window',x:x-wx,y:y-wy});
  debugPackageDrag('cursor candidates='+candidates.map(function(c){return c.source+':'+Math.round(c.x)+','+Math.round(c.y)}).join('|')+' treeTop='+Math.round(tree.getBoundingClientRect().top)+' scroll='+Math.round(tree.scrollTop));
  for(var i=0;i<candidates.length;i++){
    var c=candidates[i];
    var row=rowFromClientPoint(c.x,c.y,c.source);
    if(row)return row;
  }
  return null;
}
function droppedModelPaths(dataTransfer){
  var out=[],seen={};
  function add(value){
    var text=String(value||'').trim();
    if(!/\\.(rbxm|rbxmx)$/i.test(text))return;
    var key=text.toLowerCase();
    if(seen[key])return;
    seen[key]=1;
    out.push(text);
  }
  if(!dataTransfer)return out;
  var files=dataTransfer.files;
  if(files){
    for(var i=0;i<files.length;i++)add(files[i]&&files[i].path);
  }
  var uriList='';
  try{uriList=dataTransfer.getData('text/uri-list')||''}catch(_){uriList=''}
  uriList.split(/\\r?\\n/).forEach(function(line){
    var text=String(line||'').trim();
    if(text&&text.charAt(0)!=='#')add(text);
  });
  return out;
}
function hasExternalFileData(dataTransfer){
  if(!dataTransfer)return false;
  var types=dataTransfer.types;
  if(!types)return false;
  for(var i=0;i<types.length;i++)if(types[i]==='Files')return true;
  return false;
}
function hasPackageDragData(dataTransfer){
  if(externalPackageDrag)return true;
  if(!dataTransfer||!dataTransfer.types)return false;
  var hasText=false;
  for(var i=0;i<dataTransfer.types.length;i++){
    var type=String(dataTransfer.types[i]||'').toLowerCase();
    if(type==='application/vnd.renium.package')return true;
    if(type==='text/plain')hasText=true;
  }
  if(hasText){
    try{if(droppedPackage(dataTransfer)!==null)return true}catch(_){}
    if(!hasExternalFileData(dataTransfer))return true;
  }
  return false;
}
function droppedPackage(dataTransfer){
  if(externalPackageDrag)return externalPackageDrag;
  if(!dataTransfer)return null;
  var raw='';
  try{raw=dataTransfer.getData('application/vnd.renium.package')||''}catch(_){raw=''}
  if(!raw){
    try{raw=dataTransfer.getData('text/plain')||''}catch(_){raw=''}
  }
  raw=String(raw||'').trim();
  var prefix='renium-package:';
  if(raw.indexOf(prefix)===0)raw=raw.slice(prefix.length);
  if(!raw)return null;
  try{
    var parsed=JSON.parse(raw);
    if(parsed&&parsed.type==='renium-package'&&typeof parsed.id==='string'&&parsed.id.length>0){
      return {id:parsed.id,name:typeof parsed.name==='string'?parsed.name:parsed.id};
    }
  }catch(_){}
  return null;
}
function insertExternalPackage(targetId,reason){
  if(!externalPackageDrag||!targetId)return false;
  var pkg=externalPackageDrag;
  debugPackageDrag(reason+': inserting '+pkg.id+' mode='+(pkg.mode||'')+' into '+targetId);
  expanded.add(targetId);save();
  vscode.postMessage({type:'insertPackage',nodeId:targetId,linkId:pkg.id,name:pkg.name});
  externalPackageDrag=null;
  stopDragAutoScroll();
  clearDropTarget();
  render();
  return true;
}
function requestLoad(id,force){
  var n=nodes[id];if(!n||loadingIds[id])return;
  if(!force&&!n.hasChildren)return;
  var anchor=captureScrollAnchor(id);
  var delayUntil=loadDelayUntil[id]||0;
  if(delayUntil>Date.now()){
    setTimeout(function(){requestLoad(id,force)},delayUntil-Date.now());
    return;
  }
  loadingIds[id]=true;
  vscode.postMessage({type:'expandNode',nodeId:id,mode:filter?'search':'normal',start:visibleStart(),count:visibleCount()});
  render(anchor);
}
function compactText(value){return String(value===undefined||value===null?'':value).toLowerCase().replace(/\\s+/g,'')}
function displayText(value){
  if(value===undefined||value===null)return '';
  if(typeof value==='string'||typeof value==='number'||typeof value==='boolean')return String(value);
  if(value&&typeof value==='object'&&!Array.isArray(value)&&value._type==='EnumItem')return String(value.name||'');
  try{return JSON.stringify(value)}catch(_){return String(value)}
}
function tokenize(query){
  var out=[],re=/"([^"]*)"|'([^']*)'|(\\S+)/g,m;
  while((m=re.exec(query))!==null)out.push(m[1]!==undefined?m[1]:m[2]!==undefined?m[2]:m[3]);
  return out;
}
function splitOr(tokens){
  var groups=[[]];
  tokens.forEach(function(token){
    if(token.toLowerCase()==='or')groups.push([]);
    else if(token.toLowerCase()!=='and')groups[groups.length-1].push(token.replace(/^\\(+|\\)+$/g,''));
  });
  return groups.filter(function(group){return group.length>0});
}
function resetSearchResults(){searchResultsFilter=null;searchVisibleSet=new Set();searchResultIds=[];subtreeMatchCache={}}
function invalidateSearchCache(){searchPlanFilter=null;searchPlanGroups=[];selfMatchCache={};resetSearchResults()}
function invalidateSearchIndex(){searchIndexDirty=true;searchEntries={};searchEntryIds=[];invalidateSearchCache()}
function searchGroups(){
  if(searchPlanFilter!==filter){
    searchPlanFilter=filter;
    searchPlanGroups=filter?splitOr(tokenize(filter)):[];
    selfMatchCache={};
    resetSearchResults();
  }
  return searchPlanGroups;
}
function searchableRecord(n){
  var s=n.search||{},props={};
  function copyRecord(record){
    if(Array.isArray(record)){
      for(var i=0;i<record.length;i++){
        var pair=record[i];
        if(pair&&pair.length>=2&&props[pair[0]]===undefined)props[pair[0]]=pair[1];
      }
      return;
    }
    Object.keys(record||{}).forEach(function(key){if(props[key]===undefined)props[key]=record[key]});
  }
  copyRecord(s.properties||{});
  copyRecord(s.attributes||{});
  props.Name=s.name||n.name;props.ClassName=s.className||n.className;props.Parent=(s.path||'').split('.').slice(-2,-1)[0]||'';
  return props;
}
function buildSearchEntry(n){
  var s=n.search||{},props={};
  function addRecord(record){
    if(Array.isArray(record)){
      for(var i=0;i<record.length;i++){
        var pair=record[i];
        if(pair&&pair.length>=2)props[compactText(pair[0])]=pair[1];
      }
      return;
    }
    Object.keys(record||{}).forEach(function(key){
      var value=record[key],compactKey=compactText(key);
      props[compactKey]=value;
    });
  }
  addRecord(s.properties||{});
  addRecord(s.attributes||{});
  props.name=s.name||n.name;
  props.classname=s.className||n.className;
  props.parent=(s.path||'').split('.').slice(-2,-1)[0]||'';
  var pathParts=String(s.path||'').split('.').filter(Boolean).map(compactText);
  var classChain=(Array.isArray(s.classChain)&&s.classChain.length?s.classChain:[n.className]).map(compactText);
  var tags=(Array.isArray(s.tags)?s.tags:[]).map(compactText);
  return {
    id:n.id||n.treeId||n.name,
    name:compactText(s.name||n.name),
    className:compactText(s.className||n.className),
    classChain:classChain,
    pathParts:pathParts,
    tags:tags,
    props:props
  };
}
function ensureSearchIndex(){
  if(!searchIndexDirty)return;
  searchEntries={};
  searchEntryIds=[];
  var ids=Object.keys(nodes);
  for(var i=0;i<ids.length;i++){
    var id=ids[i],n=nodes[id];
    if(n){searchEntries[id]=buildSearchEntry(n);searchEntryIds.push(id)}
  }
  searchIndexDirty=false;
  resetSearchResults();
}
function searchEntryFor(n){
  ensureSearchIndex();
  return n?searchEntries[n.id||n.treeId||n.name]:undefined;
}
function findProperty(n,name){
  var wanted=compactText(name),entry=searchEntryFor(n);
  if(entry&&Object.prototype.hasOwnProperty.call(entry.props,wanted))return entry.props[wanted];
  var record=searchableRecord(n),keys=Object.keys(record);
  for(var i=0;i<keys.length;i++)if(compactText(keys[i])===wanted)return record[keys[i]];
  return undefined;
}
function propertyCompare(n,prop,op,expected){
  var actual=findProperty(n,prop);
  if(actual===undefined)return false;
  var aNum=Number(displayText(actual)),eNum=Number(expected);
  if((op==='<'||op==='>'||op==='<='||op==='>=')&&isFinite(aNum)&&isFinite(eNum)){
    if(op==='<')return aNum<eNum;
    if(op==='>')return aNum>eNum;
    if(op==='<=')return aNum<=eNum;
    return aNum>=eNum;
  }
  var actualText=compactText(displayText(actual)),expectedText=compactText(expected);
  if(op==='!='||op==='~=')return actualText.indexOf(expectedText)<0;
  return actualText.indexOf(expectedText)>=0;
}
function tagMatch(n,term){
  var entry=searchEntryFor(n),tags=entry?entry.tags:((n.search&&n.search.tags)||[]).map(compactText);
  term=compactText(term);
  for(var i=0;i<tags.length;i++)if(tags[i].indexOf(term)>=0)return true;
  return false;
}
function classMatch(n,term){
  var entry=searchEntryFor(n),chain=entry?entry.classChain:((n.search&&n.search.classChain)||[n.className]).map(compactText);
  term=compactText(term);
  for(var i=0;i<chain.length;i++)if(chain[i]===term)return true;
  return false;
}
function ancestryMatch(n,pattern){
  var entry=searchEntryFor(n),path=entry?entry.pathParts:((n.search&&n.search.path)||'').split('.').filter(Boolean).map(compactText);
  var parts=pattern.split('.').filter(Boolean).map(compactText);
  if(parts.length===0||path.length===0)return false;
  function at(pi,si){
    if(pi===parts.length)return si===path.length;
    if(parts[pi]==='**')return true;
    if(si>=path.length)return false;
    if(parts[pi]==='*'||parts[pi]===path[si])return at(pi+1,si+1);
    return false;
  }
  for(var start=0;start<path.length;start++)if(at(0,start))return true;
  return false;
}
function nameMatch(n,term){
  var entry=searchEntryFor(n);
  return (entry?entry.name:compactText((n.search&&n.search.name)||n.name)).indexOf(compactText(term))>=0;
}
function tokenMatch(n,tokens,index){
  var token=tokens[index]||'',next=tokens[index+1],next2=tokens[index+2];
  if(!token)return {ok:true,next:index+1};
  var colon=token.indexOf(':');
  if(colon>0){
    var prefix=token.slice(0,colon).toLowerCase(),value=token.slice(colon+1);
    if(prefix==='is')return {ok:classMatch(n,value),next:index+1};
    if(prefix==='tag')return {ok:tagMatch(n,value),next:index+1};
  }
  if(next&&(next==='='||next==='=='||next==='!='||next==='~='||next==='<'||next==='>'||next==='<='||next==='>=')){
    return {ok:propertyCompare(n,token,next,next2||''),next:index+3};
  }
  var inline=token.match(/^([^=!<>~]+)(==|=|!=|~=|<=|>=|<|>)(.+)$/);
  if(inline)return {ok:propertyCompare(n,inline[1],inline[2],inline[3]),next:index+1};
  if(token.indexOf('.')>=0||token==='*'||token==='**')return {ok:ancestryMatch(n,token),next:index+1};
  return {ok:nameMatch(n,token),next:index+1};
}
function matchesGroup(n,tokens){
  for(var i=0;i<tokens.length;){
    var result=tokenMatch(n,tokens,i);
    if(!result.ok)return false;
    i=Math.max(result.next,i+1);
  }
  return true;
}
function matchesSelf(n){
  if(!filter)return true;
  if(n.search&&n.search.hostMatch===true)return true;
  var id=n.id||n.treeId||n.name;
  if(Object.prototype.hasOwnProperty.call(selfMatchCache,id))return selfMatchCache[id];
  var groups=searchGroups(),ok=false;
  for(var i=0;i<groups.length;i++)if(matchesGroup(n,groups[i])){ok=true;break}
  selfMatchCache[id]=ok;
  return ok;
}
function fastNameGroups(){
  var groups=searchGroups();
  if(!groups.length)return null;
  var out=[];
  for(var i=0;i<groups.length;i++){
    var group=groups[i],terms=[];
    for(var j=0;j<group.length;j++){
      var token=group[j];
      if(!token||token.indexOf(':')>=0||/[=!<>~]/.test(token)||token.indexOf('.')>=0||token==='*'||token==='**')return null;
      terms.push(compactText(token));
    }
    if(!terms.length)return null;
    out.push(terms);
  }
  return out;
}
function fastNameMatch(entry,groups){
  for(var i=0;i<groups.length;i++){
    var terms=groups[i],ok=true;
    for(var j=0;j<terms.length;j++){
      if(entry.name.indexOf(terms[j])<0){ok=false;break}
    }
    if(ok)return true;
  }
  return false;
}
function markSearchVisible(n){
  while(n){
    searchVisibleSet.add(n.id);
    if(!n.parentId)break;
    n=nodes[n.parentId];
  }
}
function ensureSearchResults(){
  if(!filter){
    if(searchResultsFilter!==filter)resetSearchResults();
    searchResultsFilter=filter;
    return;
  }
  if(searchResultsFilter===filter&&!searchIndexDirty)return;
  ensureSearchIndex();
  var fastGroups=fastNameGroups();
  searchVisibleSet=new Set();
  searchResultIds=[];
  for(var i=0;i<searchEntryIds.length;i++){
    var id=searchEntryIds[i],n=nodes[id];
    if(!n)continue;
    var ok=fastGroups?fastNameMatch(searchEntries[id],fastGroups):matchesSelf(n);
    if(fastGroups)selfMatchCache[id]=ok;
    if(ok){
      searchResultIds.push(id);
      markSearchVisible(n);
    }
  }
  searchResultsFilter=filter;
}
function matches(id){
  var n=nodes[id]; if(!n)return false;
  if(!filter)return true;
  ensureSearchResults();
  return searchVisibleSet.has(id);
}
function isSearchOpen(id){
  if(!filter)return expanded.has(id);
  return !searchExpanded.has(id);
}
function collectMatchIds(id,out){
  if(!filter)return;
  ensureSearchResults();
  for(var i=0;i<searchResultIds.length;i++)out.push(searchResultIds[i]);
}
function updateSearchMeta(){
  if(!filter){
    searchMeta.classList.remove('active');
    return;
  }
  searchMeta.classList.add('active');
  if(searchLoading){
    var progress=searchTotal?('Loading '+searchLoaded+'/'+searchTotal+'... '):'Loading... ';
    searchSummary.textContent=searchMatchCount?searchMatchCount+' '+(searchMatchCount===1?'match':'matches'):progress;
  }else{
    searchSummary.textContent=searchMatchCount+' '+(searchMatchCount===1?'match':'matches');
  }
}
function collectRow(id,depth,out){
  var n=nodes[id]; if(!n||!matches(id))return;
  var kids=n.children||[],has=n.hasChildren||kids.length>0,open=isSearchOpen(id);
  out.push({type:'node',id:id,depth:depth});
  if(open){
    if(has&&kids.length===0){
      if(!loadingIds[id])autoLoadIds.push(id);
      out.push({type:'placeholder',loadId:id,depth:depth+1});
    }
    for(var i=0;i<kids.length;i++)collectRow(kids[i],depth+1,out);
  }
}
function rowHtml(item){
  if(item.type==='loading'){
    return '<div class="row placeholder" style="padding-left:'+(item.depth*12)+'px"></div>';
  }
  if(item.type==='placeholder'){
    return '<div class="row placeholder" data-load="'+esc(item.loadId)+'" style="padding-left:'+(item.depth*12)+'px"><span class="twisty leaf"></span><span class="name">Loading...</span></div>';
  }
  var id=item.id,n=nodes[id]||item; if(!n)return '';
  var has=!!n.hasChildren,open=!!n.expanded,renaming=renameId===id;
  var reniumState=directReniumState(n),reniumClass=reniumState?' renium-'+reniumState.kind:'';
  var out=[];
  out.push('<div class="row'+reniumClass+(selectedId===id?' selected':'')+(referencePreviewId===id?' reference-preview':'')+(allMatchesSelected&&n.matched?' match-selected':'')+(dropId===id?' drop-target':'')+(n.disabled?' disabled':'')+'" data-id="'+esc(id)+'" draggable="'+(!renaming&&canDrag(n)?'true':'false')+'" style="padding-left:'+(item.depth*12)+'px">');
  out.push('<span class="twisty '+(has?(open?'open':''):'leaf')+'"></span>');
  out.push('<img class="icon" src="'+ASSET+'/'+esc(n.iconName||iconName(n.className))+'.png">');
  if(renaming){
    out.push('<input class="rename" spellcheck="false" draggable="false" value="'+esc(n.name)+'">');
  }else{
    out.push('<span class="labelWrap"><span class="name">'+esc(n.name)+'</span>'+reniumBadgeHtml(reniumState)+'<button class="addBtn" type="button" title="Add child" aria-label="Add child"></button></span>');
  }
  out.push('</div>');
  return out.join('');
}
function renderFlatRows(){
  if(totalRows===0){
    tree.innerHTML=currentEmptyHtml;
    return;
  }
  var out=[];
  if(rowWindowStart>0)out.push('<div style="height:'+(rowWindowStart*ROW_HEIGHT)+'px"></div>');
  for(var i=0;i<flatRows.length;i++){
    var html=rowHtml(flatRows[i]);
    if(html)out.push(html);
  }
  var remaining=Math.max(0,totalRows-rowWindowStart-flatRows.length);
  if(remaining>0)out.push('<div style="height:'+(remaining*ROW_HEIGHT)+'px"></div>');
  tree.innerHTML=out.join('');
}
function scheduleVisibleRows(){
  updateScrollVelocity();
  if(visibleRenderFrame||renameId)return;
  visibleRenderFrame=requestAnimationFrame(function(){
    visibleRenderFrame=0;
    requestRows(false);
    schedulePrefetch();
  });
}
function stopDragAutoScroll(){
  dragAutoScrollDirection=0;
  if(dragAutoScrollFrame){
    cancelAnimationFrame(dragAutoScrollFrame);
    dragAutoScrollFrame=0;
  }
}
function startDragAutoScroll(){
  if(dragAutoScrollFrame||!dragAutoScrollDirection)return;
  dragAutoScrollFrame=requestAnimationFrame(function tick(){
    dragAutoScrollFrame=0;
    if(!draggedId||!dragAutoScrollDirection)return;
    var rect=tree.getBoundingClientRect();
    var threshold=Math.max(24,Math.min(56,rect.height*0.12));
    var distance=dragAutoScrollDirection<0
      ? Math.max(0,dragAutoScrollPointerY-rect.top)
      : Math.max(0,rect.bottom-dragAutoScrollPointerY);
    var strength=Math.max(0,Math.min(1,(threshold-distance)/threshold));
    if(strength<=0)return;
    var maxScroll=Math.max(8,ROW_HEIGHT*0.9);
    var delta=Math.max(2,Math.round(maxScroll*strength))*dragAutoScrollDirection;
    var previousTop=tree.scrollTop;
    var nextTop=Math.max(0,Math.min(tree.scrollHeight-tree.clientHeight,previousTop+delta));
    if(nextTop!==previousTop){
      tree.scrollTop=nextTop;
      scheduleVisibleRows();
    }
    if(draggedId&&dragAutoScrollDirection){
      startDragAutoScroll();
    }
  });
}
function updateDragAutoScroll(clientY){
  dragAutoScrollPointerY=clientY;
  if(!draggedId){
    stopDragAutoScroll();
    return;
  }
  var rect=tree.getBoundingClientRect();
  var threshold=Math.max(24,Math.min(56,rect.height*0.12));
  if(clientY<=rect.top+threshold)dragAutoScrollDirection=-1;
  else if(clientY>=rect.bottom-threshold)dragAutoScrollDirection=1;
  else dragAutoScrollDirection=0;
  if(dragAutoScrollDirection)startDragAutoScroll();
  else stopDragAutoScroll();
}
function render(anchor){
  var scrollAnchor=anchor||captureScrollAnchor();
  currentEmptyHtml=filter?'<div id="treeEmpty">No matches found.</div>':'<div id="treeEmpty">No services found in src.</div>';
  renderFlatRows();
  updateSearchMeta();
  restoreScrollAnchor(scrollAnchor);
  renderFlatRows();
  if(renameId){setTimeout(function(){var el=rowEl(renameId);var input=el&&el.querySelector('.rename');if(input){input.focus();input.select()}},0)}
}
function scheduleRender(anchor){
  if(anchor)pendingRenderAnchor=anchor;
  if(renderFrame)return;
  renderFrame=requestAnimationFrame(function(){
    renderFrame=0;
    var nextAnchor=pendingRenderAnchor;
    pendingRenderAnchor=null;
    render(nextAnchor);
  });
}
function applySelection(id,post){
  var preview=referencePreviewId;
  referencePreviewId=null;
  if(preview&&preview!==id){
    var previewRow=rowEl(preview);
    if(previewRow)previewRow.classList.remove('reference-preview');
  }
  var previous=selectedId;
  selectedId=id;
  save();
  if(previous&&previous!==id){
    var old=rowEl(previous);
    if(old)old.classList.remove('selected');
  }
  var current=rowEl(id);
  if(current)current.classList.add('selected');else render();
  if(externalPackageDrag&&!draggedId)markDropTarget(id);
  if(post){lastHostSelectionId=id;vscode.postMessage({type:'selectNode',nodeId:id})}
}
function clearSelection(){
  var previous=selectedId,preview=referencePreviewId;
  selectedId=null;
  referencePreviewId=null;
  lastHostSelectionId=null;
  closeMenus();
  save();
  if(previous){
    var old=rowEl(previous);
    if(old)old.classList.remove('selected');
  }
  if(preview){
    var previewRow=rowEl(preview);
    if(previewRow)previewRow.classList.remove('reference-preview');
  }
}
function scrollToId(id){
  var index=flatRowIndex(id);
  if(index>=0){
    var top=index*ROW_HEIGHT,bottom=top+ROW_HEIGHT;
    if(top<tree.scrollTop)tree.scrollTop=top;
    else if(bottom>tree.scrollTop+tree.clientHeight)tree.scrollTop=Math.max(0,bottom-tree.clientHeight);
    renderFlatRows();
  }
}
function selectNode(id){
  tree.focus();applySelection(id,true);
}
function startRename(id){
  var n=nodes[id];if(!n||n.canRename===false)return;
  renameId=id;renameOriginal=n.name;selectedId=id;closeMenus();render();
}
function finishRename(input,shouldRender){
  if(!renameId)return false;
  var id=renameId,value=input.value;
  var original=renameOriginal;
  renameId=null;renameOriginal='';renamePointerStartedInside=false;renameSuppressFocusoutUntil=0;
  if(shouldRender)render();
  if(value&&value!==original)vscode.postMessage({type:'renameInstance',nodeId:id,newName:value});
  return true;
}
function cancelRename(shouldRender){
  if(!renameId)return false;
  renameId=null;renameOriginal='';renamePointerStartedInside=false;renameSuppressFocusoutUntil=0;
  if(shouldRender)render();
  return true;
}
function currentRenameInput(){
  if(!renameId)return null;
  return tree.querySelector('.row[data-id="'+CSS.escape(renameId)+'"] .rename');
}
function keepRenameInputFocused(){
  var expectedId=renameId;
  setTimeout(function(){
    if(!expectedId||renameId!==expectedId)return;
    var input=currentRenameInput();
    if(input&&document.activeElement!==input)input.focus();
  },0);
}
function cleanupStaleRenameInput(){
  if(!renameId&&tree.querySelector('.rename'))render();
}
function finishPointerRenameCleanup(){
  suppressRenameFocusoutRender=false;
  renamePointerStartedInside=false;
  renameSuppressFocusoutUntil=0;
  cleanupStaleRenameInput();
}
function renderClassList(){
  var q=classSearch.value.trim().toLowerCase();
  var ordered=orderedClassNamesForParent(addParentId),found=[];
  for(var i=0;i<ordered.length;i++){
    var name=ordered[i];
    if(!q||name.toLowerCase().indexOf(q)>=0)found.push(name);
  }
  if(classActive>=found.length)classActive=0;
  var html='';
  for(var j=0;j<found.length;j++){
    html+='<div class="classItem'+(j===classActive?' active':'')+'" data-class="'+esc(found[j])+'"><img class="icon" src="'+ASSET+'/'+esc(iconName(found[j]))+'.png"><span>'+esc(found[j])+'</span></div>';
  }
  classList.innerHTML=html||'<div class="classItem">No classes</div>';
}
function showClassPicker(x,y,parentId){
  addParentId=parentId;classActive=0;classSearch.value='';renderClassList();
  var left=Math.max(4,Math.min(x,window.innerWidth-250));
  var top=Math.max(4,Math.min(y,window.innerHeight-330));
  classPicker.style.left=left+'px';classPicker.style.top=top+'px';classPicker.classList.remove('hidden');
  setTimeout(function(){classSearch.focus()},0);
}
function showClassPickerForNode(id){
  var el=rowEl(id),rect=el?el.getBoundingClientRect():tree.getBoundingClientRect();
  showClassPicker(rect.left+18,rect.top+22,id);
}
function showClassPickerForButton(button,id){
  var rect=button.getBoundingClientRect();
  showClassPicker(rect.left-8,rect.bottom+4,id);
}
function createClass(className){
  if(!addParentId||!className)return;
  expanded.add(addParentId);save();
  vscode.postMessage({type:'createInstance',nodeId:addParentId,className:className,name:className});
  classPicker.classList.add('hidden');
}
window.addEventListener('message',function(e){
  var m=e.data;
  if(typeof m.hasClipboardInstance==='boolean')hasClipboardInstance=!!m.hasClipboardInstance;
  if(m.type==='rbsyncTree'){rbsyncOnMessage(m);return}
  if(m.type==='prepareReferencePreview'){prepareReferencePreview();return}
  if(m.type==='packageDrag'){
    var link=m.link;
    externalPackageDrag=link&&typeof link.id==='string'&&link.id.length>0
      ? {id:link.id,name:typeof link.name==='string'?link.name:link.id,mode:typeof link.mode==='string'?link.mode:'armed'}
      : null;
    packageDragCursorSawDown=false;
    debugPackageDrag(externalPackageDrag?'received packageDrag '+externalPackageDrag.id+' mode='+externalPackageDrag.mode:'received packageDrag clear');
    if(externalPackageDrag){
      markPackageFallbackTarget('packageDrag armed');
      render();
    }
    else{
      clearDropTarget();
      render();
    }
    return;
  }
  if(m.type==='packageDragCursor'){
    if(!externalPackageDrag||draggedId)return;
    var leftDown=m.leftButtonDown===true;
    if(externalPackageDrag.mode==='drag'&&leftDown)packageDragCursorSawDown=true;
    var r=screenCursorRow(m.screenX,m.screenY,{left:m.windowLeft,top:m.windowTop,right:m.windowRight,bottom:m.windowBottom});
    debugPackageDrag('cursor result row='+(r&&r.dataset?r.dataset.id:'none')+' screen='+m.screenX+','+m.screenY+' button='+(leftDown?1:0));
    if(r)markDropTarget(r.dataset.id);
    else markPackageFallbackTarget('cursor poll');
    if(externalPackageDrag&&externalPackageDrag.mode==='drag'&&packageDragCursorSawDown&&!leftDown){
      packageDragCursorSawDown=false;
      var targetId=r&&r.dataset?r.dataset.id:null;
      if(targetId)insertExternalPackage(targetId,'cursor release');
      else{
        debugPackageDrag('cursor release: no row, cancel package drag');
        externalPackageDrag=null;
        clearDropTarget();
        render();
        vscode.postMessage({type:'cancelPackageDrag'});
      }
    }
    return;
  }
  if(m.type==='expandInserted'){var ei=nodes[m.nodeId];if(ei){expanded.add(m.nodeId);save();requestLoad(m.nodeId,true);render()}return}
  if(m.type==='linkState'){linkKeys=m.keys||{};render();return}
  if(m.type==='optimisticDelete'){optimisticDelete(m.id);return}
  if(m.type==='clearSelection'){clearSelection();return}
  if(m.type==='setTab'){setActiveTab(m.tab,true);return}
  if(m.type==='gitState'){gitState=m.state||null;gitLoading=!!m.loading;if(activeTab==='git')renderGit();return}
  if(m.type==='updateTree'){var anchor=captureScrollAnchor();nodes=m.nodes||{};rootIds=m.rootIds||[];if(m.selectedId)lastHostSelectionId=m.selectedId;selectedId=m.selectedId||selectedId;invalidateSearchIndex();Object.keys(loadingIds).forEach(function(id){var n=nodes[id];if(!n||n.loaded||(n.children&&n.children.length>0))delete loadingIds[id]});save();scheduleRender(anchor);syncSelectionToHost()}
  else if(m.type==='rowsWindow'){
    if(m.scrollToReferencePreview)prepareReferencePreview();
    var expectedMode=filter?'search':'normal';
    if(m.mode&&m.mode!==expectedMode)return;
    if(filter&&typeof m.revision==='number'&&m.revision!==searchRevision)return;
    backendErrorRetryCount=0;
    if(rowCacheMode!==expectedMode)resetRowCache(expectedMode);
    var anchor=captureScrollAnchor();
    rowRequestPending=false;
    rowWindowStart=typeof m.start==='number'?m.start:0;
    totalRows=typeof m.totalRows==='number'?m.totalRows:0;
    var receivedRows=Array.isArray(m.rows)?m.rows:[];
    for(var rowIndex=0;rowIndex<receivedRows.length;rowIndex++){
      rowCache[rowWindowStart+rowIndex]=receivedRows[rowIndex];
    }
    rememberRows(receivedRows);
    pruneRowCache(rowWindowStart,Math.max(lastRequestedCount,receivedRows.length));
    flatRows=cachedWindow(rowWindowStart,visibleCount());
    if(m.selectedId)selectedId=m.selectedId;
    referencePreviewId=typeof m.referencePreviewId==='string'?m.referencePreviewId:null;
    if(Array.isArray(m.matchIds))matchIds=m.matchIds;
    if(typeof m.matchCount==='number')searchMatchCount=m.matchCount;
    if(filter){searchLoading=false;searchInitialLoading=false}
    searchLoaded=typeof m.loaded==='number'?m.loaded:searchLoaded;
    searchTotal=typeof m.total==='number'?m.total:searchTotal;
    currentEmptyHtml=filter?'<div id="treeEmpty">No matches found.</div>':'<div id="treeEmpty">No services found in src.</div>';
    save();render(anchor);syncSelectionToHost();
    if(m.scrollToReferencePreview&&referencePreviewId){setTimeout(function(){scrollToId(referencePreviewId)},0)}
    if(m.scrollToSelected&&selectedId){setTimeout(function(){scrollToId(selectedId);if(document.activeElement===search||Date.now()<searchRetainFocusUntil){if(document.activeElement!==search)searchRestoringFocus=true;search.focus();return}tree.focus()},0)}
    schedulePrefetch();
  }
  else if(m.type==='rowsPrefetch'){
    var expectedPrefetchMode=filter?'search':'normal';
    prefetchPending=false;
    if(m.mode&&m.mode!==expectedPrefetchMode)return;
    if(filter&&typeof m.revision==='number'&&m.revision!==searchRevision)return;
    if(rowCacheMode!==expectedPrefetchMode)return;
    var prefetchStart=typeof m.start==='number'?m.start:0;
    var prefetchRows=Array.isArray(m.rows)?m.rows:[];
    if(typeof m.totalRows==='number')totalRows=m.totalRows;
    var currentCount=lastRequestedCount||visibleCount();
    var affectsVisible=prefetchStart<rowWindowStart+currentCount&&prefetchStart+prefetchRows.length>rowWindowStart;
    for(var pi=0;pi<prefetchRows.length;pi++)rowCache[prefetchStart+pi]=prefetchRows[pi];
    rememberRows(prefetchRows);
    pruneRowCache(rowWindowStart,Math.max(currentCount,prefetchRows.length));
    if(affectsVisible){
      flatRows=cachedWindow(rowWindowStart,currentCount);
      renderFlatRows();
    }
    schedulePrefetch();
  }
	  else if(m.type==='rowsPrefetchDone'){prefetchPending=false;schedulePrefetch()}
	  else if(m.type==='invalidateRows'){lastRequestedStart=-1;requestRows(true)}
	  else if(m.type==='loadComplete'){var completeAnchor=captureScrollAnchor(m.nodeId);delete loadingIds[m.nodeId];if(m.ok===false)loadDelayUntil[m.nodeId]=Date.now()+1200;else delete loadDelayUntil[m.nodeId];render(completeAnchor)}
	  else if(m.type==='searchStatus'){searchLoading=!!m.loading;if(!searchLoading)searchInitialLoading=false;searchLoaded=typeof m.loaded==='number'?m.loaded:searchLoaded;searchTotal=typeof m.total==='number'?m.total:searchTotal;if(typeof m.matchCount==='number')searchMatchCount=m.matchCount;updateSearchMeta()}
	  else if(m.type==='historyEntries'){historyLoading=false;historyLoaded=true;historyGroups=Array.isArray(m.groups)?m.groups:(Array.isArray(m.entries)?m.entries.map(function(entry){return{id:entry.id,title:entry.targetLabel,subtitle:historyMetaText(entry),entryCount:1,targetCount:1,items:[entry]}}):[]);historyRestoring={};renderHistory()}
	  else if(m.type==='historyError'){historyLoading=false;historyLoaded=true;historyList.innerHTML='<div id="treeEmpty">'+esc(m.message||'Failed to load history.')+'</div>'}
	  else if(m.type==='historyRestoreComplete'){if(m.id)delete historyRestoring[m.id];if(m.groupId)delete historyRestoring[m.groupId];renderHistory()}
	  else if(m.type==='clipboardState'){if(!hasClipboardInstance)menu.classList.add('hidden')}
	  else if(m.type==='error'){
    var message=m.message||'Explorer failed to load.';
    rowRequestPending=false;
    lastRequestedStart=-1;
    if(/Explorer backend exited|timed out|not running/i.test(message)&&backendErrorRetryCount<2){
      backendErrorRetryCount++;
      setTimeout(function(){requestRows(true)},180);
      return;
    }
    searchLoading=false;searchInitialLoading=false;searchRequested=false;updateSearchMeta();tree.innerHTML='<div id="treeEmpty">'+esc(message)+'</div>';save()
  }
});
function startSearchLoad(force){
  if(!filter)return;
  if(force||!searchRequested){
    searchRequested=true;searchLoading=true;searchInitialLoading=true;searchLoaded=0;searchTotal=0;searchMatchCount=0;updateSearchMeta();
    if(searchDebounce)clearTimeout(searchDebounce);
    searchDebounce=setTimeout(function(){
      searchDebounce=null;
      lastRequestedStart=-1;
      lastRequestMode='search';
      tree.scrollTop=0;
      currentEmptyHtml='<div id="treeEmpty"></div>';
      if(totalRows===0){renderFlatRows()}
      vscode.postMessage({type:'searchLoad',query:filter,start:0,count:visibleCount(),mode:'search',revision:searchRevision});
    },force?0:23);
  }
}
search.addEventListener('input',function(){
  var nextFilter=search.value.trim().toLowerCase();
  var wasFiltering=!!filter;
  if(nextFilter!==filter){searchRevision++;prefetchPending=false;if(prefetchTimer){clearTimeout(prefetchTimer);prefetchTimer=null}searchInitialLoading=false;searchExpanded.clear();searchRequested=false;matchIds=[];resetRowCache(nextFilter?'search':'normal');rowWindowStart=0;totalRows=0;flatRows=[];lastRequestedStart=-1;lastRequestMode=nextFilter?'search':'normal'}
  filter=nextFilter;allMatchesSelected=false;invalidateSearchCache();
  hideSearchSuggestions();
  if(filter)startSearchLoad(false);else{if(searchDebounce)clearTimeout(searchDebounce);var hadFocus=document.activeElement===search;if(hadFocus)searchRetainFocusUntil=Date.now()+1500;searchLoading=false;searchInitialLoading=false;searchRequested=false;searchLoaded=0;searchTotal=0;searchMatchCount=0;rowWindowStart=0;totalRows=0;flatRows=[];tree.scrollTop=0;lastRequestedStart=-1;lastRequestMode='normal';resetRowCache('normal');currentEmptyHtml='<div id="treeEmpty">Loading...</div>';renderFlatRows();vscode.postMessage({type:'clearSearch',start:0,count:visibleCount(),mode:'normal'});if(wasFiltering&&selectedId)expandAncestors(selectedId);if(hadFocus)showSearchSuggestionsOnce();if(hadFocus)setTimeout(function(){if(document.activeElement!==search)searchRestoringFocus=true;search.focus()},0)}
  updateSearchMeta();
});
tree.addEventListener('scroll',scheduleVisibleRows);
tree.addEventListener('wheel',function(e){
  if(e.deltaY){
    scrollDirection=e.deltaY<0?-1:1;
    schedulePrefetch();
  }
},{passive:true});
search.addEventListener('mousedown',function(){searchPointerOpenUntil=Date.now()+600});
search.addEventListener('focus',function(){if(searchRestoringFocus){searchRestoringFocus=false;return}searchSuggestionsShownThisFocus=false;showSearchSuggestionsOnce()});
search.addEventListener('blur',function(){
  setTimeout(function(){
    if(document.activeElement===search)return;
    if(Date.now()<searchPointerOpenUntil){
      if(suggestions.classList.contains('active'))return;
    }
    hideSearchSuggestions();
    searchSuggestionsShownThisFocus=false;
  },80);
});
suggestions.addEventListener('mousedown',function(e){searchPointerOpenUntil=Date.now()+600;e.preventDefault()});
suggestions.addEventListener('click',function(e){
  var item=e.target.closest('.suggestItem');if(!item)return;
  search.value=item.dataset.insert||'';searchRevision++;filter=search.value.trim().toLowerCase();searchRequested=false;searchInitialLoading=false;prefetchPending=false;matchIds=[];resetRowCache(filter?'search':'normal');rowWindowStart=0;totalRows=0;flatRows=[];lastRequestedStart=-1;invalidateSearchCache();hideSearchSuggestions();search.focus();startSearchLoad(false);render();
});
function jumpMatch(delta){
  if(matchIds.length===0)return;
  var current=selectedId?matchIds.indexOf(selectedId):-1;
  if(current<0)current=delta>0?-1:0;
  var next=(current+delta+matchIds.length)%matchIds.length;
  selectNode(matchIds[next]);
  scrollToId(matchIds[next]);
  var row=rowEl(matchIds[next]);if(row)row.scrollIntoView({block:'nearest'});
}
prevMatch.addEventListener('click',function(){jumpMatch(-1)});
nextMatch.addEventListener('click',function(){jumpMatch(1)});
selectMatches.addEventListener('click',function(){allMatchesSelected=true;render()});
refreshResults.addEventListener('click',function(){searchRequested=false;startSearchLoad(true);render()});
tabs.addEventListener('click',function(e){
  var btn=e.target.closest('.tabBtn');if(!btn)return;
  setActiveTab(btn.dataset.tab);
});
var rbTree=null,rbExpanded={},rbSelId=null,rbById={};
var rbQuery='',rbSearchCollapsed={},rbFlat=[],rbSizer=null,rbRowsEl=null,rbPaintQueued=false;
var RB_ROW_H=22,RB_OVER=6;
function rbIndex(){rbById={};function w(n){rbById[n.settingsId]=n;if(n.children)for(var i=0;i<n.children.length;i++)w(n.children[i])}var r=(rbTree&&rbTree.roots)||[];for(var i=0;i<r.length;i++)w(r[i])}
function rbHi(name){
  var s=String(name==null?'':name);
  if(!rbQuery)return esc(s);
  var i=s.toLowerCase().indexOf(rbQuery);
  if(i<0)return esc(s);
  return esc(s.slice(0,i))+'<span class="rbhi">'+esc(s.slice(i,i+rbQuery.length))+'</span>'+esc(s.slice(i+rbQuery.length));
}
function rbRowHtml(f){
  var n=f.n;
  var h='<div class="row'+(rbSelId===n.settingsId?' selected':'')+(f.match?' rbmatch':'')+'" data-rbid="'+esc(n.settingsId)+'" style="padding-left:'+(f.depth*12)+'px">';
  h+='<span class="twisty '+(f.has?(f.open?'open':''):'leaf')+'"></span>';
  h+='<img class="icon" src="'+ASSET+'/'+esc(iconName(n.className))+'.png">';
  h+='<span class="labelWrap"><span class="name">'+(f.match?rbHi(n.name):esc(n.name))+'</span></span></div>';
  return h;
}
function rbBuildFlat(){
  rbFlat=[];
  if(!rbTree)return;
  var roots=rbTree.roots||[];
  if(!rbQuery){
    (function walk(list,depth){
      for(var i=0;i<list.length;i++){
        var n=list[i],kids=n.children||[],open=!!rbExpanded[n.settingsId];
        rbFlat.push({n:n,depth:depth,has:kids.length>0,open:open,match:false});
        if(kids.length&&open)walk(kids,depth+1);
      }
    })(roots,0);
    return;
  }
  var inc={};
  function mark(n){
    var kids=n.children||[],any=false;
    for(var i=0;i<kids.length;i++){if(mark(kids[i]))any=true;}
    var self=(String(n.name)+' '+String(n.className)).toLowerCase().indexOf(rbQuery)>=0;
    if(self||any){inc[n.settingsId]=self?2:1;return true;}
    return false;
  }
  for(var i=0;i<roots.length;i++)mark(roots[i]);
  (function walk(list,depth){
    for(var j=0;j<list.length;j++){
      var n=list[j],flag=inc[n.settingsId];
      if(!flag)continue;
      var kids=n.children||[],open=!rbSearchCollapsed[n.settingsId];
      rbFlat.push({n:n,depth:depth,has:kids.length>0,open:open,match:flag===2});
      if(kids.length&&open)walk(kids,depth+1);
    }
  })(roots,0);
}
function rbEnsureShell(){
  if(rbSizer&&rbSizer.parentNode===rbsyncTree)return;
  rbsyncTree.innerHTML='';
  rbSizer=document.createElement('div');rbSizer.className='rbSizer';
  rbRowsEl=document.createElement('div');rbRowsEl.className='rbRows';
  rbSizer.appendChild(rbRowsEl);rbsyncTree.appendChild(rbSizer);
}
function rbPaint(){
  if(!rbTree||!rbFlat.length)return;
  rbEnsureShell();
  var total=rbFlat.length;
  rbSizer.style.height=(total*RB_ROW_H)+'px';
  var vh=rbsyncTree.clientHeight||300,scrollTop=rbsyncTree.scrollTop;
  var start=Math.max(0,Math.floor(scrollTop/RB_ROW_H)-RB_OVER);
  var end=Math.min(total,Math.ceil((scrollTop+vh)/RB_ROW_H)+RB_OVER);
  var out=[];for(var i=start;i<end;i++)out.push(rbRowHtml(rbFlat[i]));
  rbRowsEl.style.transform='translateY('+(start*RB_ROW_H)+'px)';
  rbRowsEl.innerHTML=out.join('');
}
function rbSchedulePaint(){
  if(rbPaintQueued)return;
  rbPaintQueued=true;
  requestAnimationFrame(function(){rbPaintQueued=false;rbPaint()});
}
function rbWireBrowse(){var b=document.getElementById('rbsyncBrowse');if(b)b.addEventListener('click',function(e){e.preventDefault();vscode.postMessage({type:'rbsyncBrowse'})})}
function rbEmpty(){rbSizer=null;rbRowsEl=null;rbsyncTree.innerHTML='<div class="rbsyncHint">Open a <b>.renium</b> store with the folder button above, or <a id="rbsyncBrowse" href="#">browse for a file</a>.</div>';rbWireBrowse()}
function rbRender(){
  if(!rbTree||!((rbTree.roots||[]).length)){rbEmpty();return}
  rbBuildFlat();
  if(!rbFlat.length){rbSizer=null;rbRowsEl=null;rbsyncTree.innerHTML='<div class="rbsyncHint">No matches.</div>';return}
  rbPaint();
}
function rbSelect(id){
  rbSelId=id;var n=rbById[id];rbPaint();
  if(n)vscode.postMessage({type:'rbsyncSelect',node:{name:n.name,className:n.className,settingsId:n.settingsId,properties:n.properties||{},attributes:n.attributes||{}}});
}
rbsyncTree.addEventListener('scroll',rbSchedulePaint);
window.addEventListener('resize',function(){if(activeTab==='rbsync')rbSchedulePaint()});
rbsyncTree.addEventListener('click',function(e){
  if(e.target.closest('#rbsyncBrowse')){e.preventDefault();vscode.postMessage({type:'rbsyncBrowse'});return}
  var row=e.target.closest('.row');if(!row)return;
  var id=row.dataset.rbid,n=rbById[id];if(!n)return;
  var tw=e.target.closest('.twisty');
  if(tw&&!tw.classList.contains('leaf')){
    if(rbQuery)rbSearchCollapsed[id]=!rbSearchCollapsed[id];
    else rbExpanded[id]=!rbExpanded[id];
    rbRender();return;
  }
  rbSelect(id);
});
function rbSearch(term){
  if(!rbTree)return;
  var q=(term||'').trim().toLowerCase();
  if(q===rbQuery)return;
  rbQuery=q;rbSearchCollapsed={};rbsyncTree.scrollTop=0;
  rbRender();
}
rbsyncSearch.addEventListener('input',function(){rbSearch(rbsyncSearch.value)});
function rbsyncOnMessage(m){
  if(m.error){rbTree=null;rbSelId=null;rbSizer=null;rbRowsEl=null;rbsyncSearch.placeholder='Search';rbsyncTree.innerHTML='';var d=document.createElement('div');d.className='rberr';d.textContent='Could not read this file:\\n\\n'+m.error;rbsyncTree.appendChild(d);return}
  rbTree=m.result;rbExpanded={};rbSelId=null;rbQuery='';rbSearchCollapsed={};rbsyncSearch.value='';rbsyncTree.scrollTop=0;rbIndex();
  var rbBig=rbTree&&rbTree.instanceCount>800;
  (function w(l,depth){for(var i=0;i<l.length;i++){var n=l[i];if(n.children&&n.children.length&&(!rbBig||depth===0))rbExpanded[n.settingsId]=true;if(n.children)w(n.children,depth+1)}})((rbTree&&rbTree.roots)||[],0);
  rbsyncSearch.placeholder=rbTree?('Search '+rbTree.instanceCount+' instances'):'Search';
  rbRender();
  if(rbFlat.length)rbSelect(rbFlat[0].n.settingsId);
}
function rbsyncSendBytes(file){
  var maxBytes=${MAX_RBSYNC_DROPPED_BYTES};
  if(!file||file.size>maxBytes){
    rbsyncSearch.placeholder='Search';
    rbsyncTree.innerHTML='<div class="rberr">Dropped files are limited to '+Math.floor(maxBytes/(1024*1024))+' MiB. Use the file picker for a larger store.</div>';
    return;
  }
  rbsyncSearch.placeholder='decoding...';
  var reader=new FileReader();
  reader.onload=function(){var data=typeof reader.result==='string'?reader.result:'';var comma=data.indexOf(',');if(comma<0){rbsyncSearch.placeholder='Search';return}vscode.postMessage({type:'rbsyncDecode',name:file.name,base64:data.slice(comma+1)})};
  reader.onerror=function(){rbsyncSearch.placeholder='Search'};
  reader.readAsDataURL(file);
}
(function(){
  rbWireBrowse();
  var openBtn=document.getElementById('rbsyncOpen');if(openBtn)openBtn.addEventListener('click',function(){vscode.postMessage({type:'rbsyncBrowse'})});
  function active(){return activeTab==='rbsync'}
  function draggy(e){var t=(e.dataTransfer&&e.dataTransfer.types)||[];for(var i=0;i<t.length;i++){if(t[i]==='Files'||t[i]==='text/uri-list'||t[i]==='text/plain')return true}return false}
  document.addEventListener('dragenter',function(e){if(!active()||!draggy(e))return;e.preventDefault();rbsyncPane.classList.add('rbdrag')},true);
  document.addEventListener('dragover',function(e){if(!active()||!draggy(e))return;e.preventDefault();if(e.dataTransfer)e.dataTransfer.dropEffect='copy';rbsyncPane.classList.add('rbdrag')},true);
  document.addEventListener('dragleave',function(e){if(!active())return;if(e.relatedTarget)return;rbsyncPane.classList.remove('rbdrag')},true);
  document.addEventListener('drop',function(e){
    if(!active())return;
    e.preventDefault();rbsyncPane.classList.remove('rbdrag');
    var dt=e.dataTransfer;if(!dt)return;
    var file=dt.files&&dt.files[0];
    if(file){rbsyncSendBytes(file);return}
    var uri='';try{uri=dt.getData('text/uri-list')||dt.getData('text/plain')||''}catch(_){uri=''}
    if(uri){rbsyncSearch.placeholder='decoding...';vscode.postMessage({type:'rbsyncDecodePath',path:uri})}
  },true);
})();
gitApp.addEventListener('click',function(e){
  var refresh=e.target.closest('[data-gh-refresh]');
  if(refresh){
    vscode.postMessage({type:'gitRefresh'});
    return;
  }
  var group=e.target.closest('[data-gh-group]');
  if(group){
    toggleGitGroup(String(group.dataset.ghGroup||''));
    return;
  }
  var output=e.target.closest('[data-gh-output]');
  if(output){
    closeGitActions();
    vscode.postMessage({type:'gitOpenOutput'});
    return;
  }
  var action=e.target.closest('[data-gh-action]');
  if(action){
    closeGitActions();
    vscode.postMessage({type:'gitAction',action:action.dataset.ghAction});
    return;
  }
  var diff=e.target.closest('[data-gh-diff]');
  if(diff){
    vscode.postMessage({type:'gitDiff',path:diff.dataset.ghDiff});
    return;
  }
});
gitApp.addEventListener('keydown',function(e){
  if(e.key!=='Enter'&&e.key!==' ')return;
  var diff=e.target.closest('[data-gh-diff]');
  if(diff){
    e.preventDefault();
    vscode.postMessage({type:'gitDiff',path:diff.dataset.ghDiff});
  }
});
refreshHistory.addEventListener('click',function(){historyLoaded=false;loadHistory()});
historyList.addEventListener('click',function(e){
  var btn=e.target.closest('.historyAction');if(!btn||btn.disabled)return;
  e.stopPropagation();
  var id=btn.dataset.id,action=btn.dataset.action;
  if(action==='restoreHistory'){
    historyRestoring[id]=true;
    renderHistory();
    vscode.postMessage({type:action,historyId:id});
    return;
  }
  if(action==='restoreHistoryGroup'){
    var ids=[];
    try{ids=JSON.parse(btn.dataset.ids||'[]')}catch(_){}
    var groupId=btn.dataset.groupId;
    historyRestoring[groupId]=true;
    renderHistory();
    vscode.postMessage({type:action,historyGroupId:groupId,historyIds:ids});
    return;
  }
  vscode.postMessage({type:action,historyId:id});
});
historyList.addEventListener('click',function(e){
  if(e.target.closest('.historyAction'))return;
  var toggle=e.target.closest('[data-action="toggleHistoryGroup"]');
  if(toggle){
    var groupId=toggle.dataset.groupId;
    if(historyExpanded.has(groupId))historyExpanded.delete(groupId);else historyExpanded.add(groupId);
    save();renderHistory();
    return;
  }
  var child=e.target.closest('.historyChild');
  if(child&&child.dataset.openId&&!child.classList.contains('noDiff')){
    vscode.postMessage({type:'compareHistoryBackup',historyId:child.dataset.openId});
  }
});
tree.addEventListener('click',function(e){
  if(e.target.closest('.rename'))return;
  closeMenus();
  var r=e.target.closest('.row'); if(!r){finishPointerRenameCleanup();return}
  if(r.dataset.load){requestLoad(r.dataset.load,true);finishPointerRenameCleanup();return}
  var id=r.dataset.id,n=nodes[id]; if(!n){finishPointerRenameCleanup();return}
  var addBtn=e.target.closest('.addBtn');
  if(addBtn){
    e.preventDefault();
    e.stopPropagation();
    applySelection(id,true);
    showClassPickerForButton(addBtn,id);
    finishPointerRenameCleanup();
    return;
  }
  if(e.target.closest('.twisty')&&!e.target.closest('.twisty').classList.contains('leaf')){
    var anchor=captureScrollAnchor(id);
    loadingIds[id]=true;
    if(n.expanded){
      n.expanded=false;
      vscode.postMessage({type:'collapseNode',nodeId:id,mode:filter?'search':'normal',start:visibleStart(),count:visibleCount()});
    }else{
      n.expanded=true;
      vscode.postMessage({type:'expandNode',nodeId:id,mode:filter?'search':'normal',start:visibleStart(),count:visibleCount()});
    }
    suppressRenameFocusoutRender=false;
    render(anchor); return;
  }
  selectNode(id);
  finishPointerRenameCleanup();
});
tree.addEventListener('mousedown',function(e){
  if(!renameId)return;
  var r=e.target.closest('.row');
  if(!r||r.dataset.id===renameId)return;
  var input=tree.querySelector('.row[data-id="'+CSS.escape(renameId)+'"] .rename');
  if(input){
    suppressRenameFocusoutRender=true;
    finishRename(input,false);
  }
},true);
document.addEventListener('mousedown',function(e){
  if(!renameId)return;
  renamePointerStartedInside=!!(e.target&&e.target.closest&&e.target.closest('.rename'));
  if(renamePointerStartedInside)renameSuppressFocusoutUntil=Date.now()+1200;
},true);
document.addEventListener('mouseup',function(){
  if(!renamePointerStartedInside)return;
  renameSuppressFocusoutUntil=Date.now()+180;
  setTimeout(function(){renamePointerStartedInside=false},0);
},true);
tree.addEventListener('dblclick',function(e){
  rememberPointerEvent(e);
  if(e.target.closest('.rename'))return;
  var r=e.target.closest('.row');if(r){var n=nodes[r.dataset.id];if(n&&n.isScript)vscode.postMessage({type:'openScript',nodeId:r.dataset.id})}
});
tree.addEventListener('contextmenu',function(e){
  rememberPointerEvent(e);
  e.preventDefault(); var r=e.target.closest('.row'); if(!r)return;
  menuNode=r.dataset.id; menuX=e.clientX; menuY=e.clientY; tree.focus(); applySelection(menuNode,false); vscode.postMessage({type:'selectNode',nodeId:menuNode});
  var n=nodes[menuNode],html='';
  if(n&&n.isScript)html+='<div class="mi" data-c="openScript">Open Script</div>';
  if(n&&n.canRename!==false)html+='<div class="mi" data-c="renameInstance">Rename</div>';
  if(n&&n.kind!=='service')html+='<div class="mi" data-c="copyInstance">Copy</div>';
  if(hasClipboardInstance)html+='<div class="mi" data-c="pasteInstance">Paste Into</div>';
  if(n&&n.kind!=='service')html+='<div class="mi" data-c="duplicateInstance">Duplicate</div>';
  html+='<div class="mi" data-c="importModel">Import</div>';
  if(n&&n.kind!=='service')html+='<div class="mi" data-c="exportModel">Export</div>';
  var linkSt=nodeLinkState(n);
  if(n&&n.kind!=='service'&&linkSt!=='linked'&&linkSt!=='broken')html+='<div class="mi" data-c="createLink">Create Link</div>';
  if(n&&n.kind!=='service'&&(linkSt==='linked'||linkSt==='broken'))html+='<div class="mi" data-c="resaveLink">Save New Package Version</div>';
  if(n&&n.kind!=='service'&&linkSt==='linked')html+='<div class="mi" data-c="breakLink">Break Link</div>';
  if(n&&n.kind!=='service'&&linkSt==='broken')html+='<div class="mi" data-c="relinkLink">Relink Package</div>';
  if(canDesyncPackage(n))html+='<div class="mi" data-c="desyncPackageLink">Desync Roblox Package</div>';
  if(n&&n.kind!=='service'&&n.canDelete!==false)html+='<div class="mi" data-c="deleteInstance">Delete</div>';
  html+='<div class="mi" data-c="copyPath">Copy Roblox Path</div>';
  classPicker.classList.add('hidden');
  menu.innerHTML=html; menu.style.left=e.clientX+'px'; menu.style.top=e.clientY+'px'; menu.classList.remove('hidden');
});
menu.addEventListener('click',function(e){
  var i=e.target.closest('.mi');if(!i||!menuNode)return;
  var c=i.dataset.c;menu.classList.add('hidden');
  if(c==='renameInstance'){startRename(menuNode);return}
  vscode.postMessage({type:c,nodeId:menuNode});
});
classSearch.addEventListener('input',function(){classActive=0;renderClassList()});
classSearch.addEventListener('keydown',function(e){
  var items=classList.querySelectorAll('.classItem[data-class]');
  if(e.key==='ArrowDown'){e.preventDefault();classActive=Math.min(items.length-1,classActive+1);renderClassList()}
  else if(e.key==='ArrowUp'){e.preventDefault();classActive=Math.max(0,classActive-1);renderClassList()}
  else if(e.key==='Enter'){e.preventDefault();var item=items[classActive]||items[0];if(item)createClass(item.dataset.class)}
  else if(e.key==='Escape'){classPicker.classList.add('hidden');tree.focus()}
});
classList.addEventListener('click',function(e){var item=e.target.closest('.classItem[data-class]');if(item)createClass(item.dataset.class)});
tree.addEventListener('pointermove',function(e){
  rememberPointerEvent(e);
  var r=e.target.closest('.row');
  if(!r)return;
  rememberPointerRow(r);
  if(externalPackageDrag&&!draggedId)markDropTarget(r.dataset.id);
});
tree.addEventListener('keydown',function(e){
  var target=e.target;
  if(target&&(target.tagName==='INPUT'||target.tagName==='SELECT'||target.tagName==='TEXTAREA'||(target.classList&&target.classList.contains('rename'))))return;
  if(!selectedId)return;
  var selected=nodes[selectedId],key=String(e.key||'').toLowerCase();
  if((e.key==='F2'||e.key==='Enter')&&selected&&selected.canRename!==false){e.preventDefault();startRename(selectedId)}
  else if(e.key==='Delete'&&selected&&selected.kind!=='service'&&selected.canDelete!==false){e.preventDefault();vscode.postMessage({type:'deleteInstance',nodeId:selectedId})}
  else if((e.ctrlKey||e.metaKey)&&e.shiftKey&&key==='a'){e.preventDefault();showClassPickerForNode(selectedId)}
  else if((e.ctrlKey||e.metaKey)&&key==='c'&&selected&&selected.kind!=='service'){e.preventDefault();vscode.postMessage({type:'copyInstance',nodeId:selectedId})}
  else if((e.ctrlKey||e.metaKey)&&key==='v'&&hasClipboardInstance){e.preventDefault();vscode.postMessage({type:'pasteInstance',nodeId:selectedId})}
  else if((e.ctrlKey||e.metaKey)&&key==='d'&&selected&&selected.kind!=='service'){e.preventDefault();vscode.postMessage({type:'duplicateInstance',nodeId:selectedId})}
});
tree.addEventListener('dragstart',function(e){
  rememberPointerEvent(e);
  if(e.target.closest('.rename')){
    e.preventDefault();
    e.stopPropagation();
    draggedId=null;
    return;
  }
  var r=e.target.closest('.row');if(!r)return;
  var id=r.dataset.id,n=nodes[id];if(!canDrag(n)){e.preventDefault();return}
  draggedId=id;e.dataTransfer.effectAllowed='move';e.dataTransfer.setData('text/plain',id);r.classList.add('dragging');
  updateDragAutoScroll(e.clientY||0);
});
tree.addEventListener('dragover',function(e){
  rememberPointerEvent(e);
  updateDragAutoScroll(e.clientY||0);
  var r=e.target.closest('.row');
  if(r)rememberPointerRow(r);
  if(draggedId){
    if(!r)return;
    var id=r.dataset.id;if(!canDrop(draggedId,id))return;
    e.preventDefault();e.dataTransfer.dropEffect='move';
    markDropTarget(id);
    return;
  }
  if(!r){
    if(hasPackageDragData(e.dataTransfer)){
      e.preventDefault();
      if(e.dataTransfer)e.dataTransfer.dropEffect='copy';
      markPackageFallbackTarget('tree dragover');
      return;
    }
    clearDropTarget();
    return;
  }
  if(hasPackageDragData(e.dataTransfer)){
    e.preventDefault();e.dataTransfer.dropEffect='copy';
    markDropTarget(r.dataset.id);
    return;
  }
  if(!hasExternalFileData(e.dataTransfer))return;
  e.preventDefault();e.dataTransfer.dropEffect='copy';
  markDropTarget(r.dataset.id);
});
document.addEventListener('dragover',function(e){
  if(draggedId)return;
  if(!hasPackageDragData(e.dataTransfer))return;
  var r=dragEventRow(e);
  if(!r){
    debugPackageDrag('document dragover: package active but no row');
    markPackageFallbackTarget('document dragover');
    return;
  }
  e.preventDefault();
  e.stopPropagation();
  if(e.dataTransfer)e.dataTransfer.dropEffect='copy';
  markDropTarget(r.dataset.id);
},true);
tree.addEventListener('dragleave',function(e){
  if(!tree.contains(e.relatedTarget)){
    if(externalPackageDrag)markPackageFallbackTarget('tree dragleave');
    else clearDropTarget();
    stopDragAutoScroll();
  }
});
tree.addEventListener('drop',function(e){
  var r=e.target.closest('.row');
  if(r)rememberPointerRow(r);
  var targetId=(r&&r.dataset.id)||packageDropTargetFromState();
  var pkg=droppedPackage(e.dataTransfer);
  if(pkg&&targetId){
    e.preventDefault();expanded.add(targetId);save();
    vscode.postMessage({type:'insertPackage',nodeId:targetId,linkId:pkg.id,name:pkg.name});
    externalPackageDrag=null;
    stopDragAutoScroll();
    clearDropTarget();
    draggedId=null;render();
    return;
  }
  var modelPaths=droppedModelPaths(e.dataTransfer);
  if(modelPaths.length>0&&targetId){
    e.preventDefault();expanded.add(targetId);save();
    vscode.postMessage({type:'importModel',nodeId:targetId,modelPaths:modelPaths});
    stopDragAutoScroll();
    clearDropTarget();
    draggedId=null;render();
    return;
  }
  if(!r||!draggedId)return;
  targetId=r.dataset.id;if(!canDrop(draggedId,targetId))return;
  e.preventDefault();expanded.add(targetId);save();
  vscode.postMessage({type:'moveInstance',nodeId:draggedId,targetId:targetId});
  stopDragAutoScroll();
  draggedId=null;clearDropTarget();render();
});
document.addEventListener('drop',function(e){
  if(draggedId)return;
  var pkg=droppedPackage(e.dataTransfer);
  if(!pkg){debugPackageDrag('document drop: no package payload');return}
  var r=dragEventRow(e),targetId=(r&&r.dataset.id)||packageDropTargetFromState(true);
  if(!targetId){debugPackageDrag('document drop: package '+pkg.id+' but no target row/selection');return}
  e.preventDefault();
  e.stopPropagation();
  insertExternalPackage(targetId,'document drop');
},true);
document.addEventListener('mousemove',function(e){
  rememberPointerEvent(e);
  if(!externalPackageDrag||draggedId)return;
  var r=dragEventRow(e);
  if(r){
    rememberPointerRow(r);
    markDropTarget(r.dataset.id);
    if(externalPackageDrag&&externalPackageDrag.mode==='drag'&&e.buttons===0){
      insertExternalPackage(r.dataset.id,'post-release hover');
    }
  }
  else markPackageFallbackTarget('document mousemove');
},true);
document.addEventListener('click',function(e){
  rememberPointerEvent(e);
  if(!externalPackageDrag||draggedId)return;
  var r=dragEventRow(e),targetId=(r&&r.dataset.id)||packageDropTargetFromState();
  if(!targetId){debugPackageDrag('placement click: no target row/selection');return}
  e.preventDefault();
  e.stopPropagation();
  insertExternalPackage(targetId,'placement click');
},true);
tree.addEventListener('dragend',function(){stopDragAutoScroll();draggedId=null;clearDropTarget();render()});
tree.addEventListener('keydown',function(e){
  if(e.target&&e.target.classList&&e.target.classList.contains('rename')){
    if(e.key==='Enter'){
      e.preventDefault();
      e.stopPropagation();
      finishRename(e.target,true);
    }
    else if(e.key==='Escape'){
      e.preventDefault();
      e.stopPropagation();
      cancelRename(true);
    }
  }
  if(e.key==='Escape'&&externalPackageDrag){
    externalPackageDrag=null;
    clearDropTarget();
    render();
  }
},true);
tree.addEventListener('focusout',function(e){
  if(e.target&&e.target.classList&&e.target.classList.contains('rename')){
    if(renamePointerStartedInside||Date.now()<renameSuppressFocusoutUntil){
      keepRenameInputFocused();
      return;
    }
    if(suppressRenameFocusoutRender)return;
    if(!finishRename(e.target,true))cleanupStaleRenameInput();
  }
});
document.addEventListener('click',function(e){
  if(!e.target.closest('#menu')&&!e.target.closest('#classPicker')&&!e.target.closest('#suggestions')&&!e.target.closest('#bar')&&!e.target.closest('#gitPane'))closeMenus();
  if(suppressRenameFocusoutRender)finishPointerRenameCleanup();
});
setActiveTab(activeTab,true);
if(rootIds.length&&activeTab==='explorer')render();
vscode.postMessage({type:'ready'});
if(activeTab==='history')setTimeout(loadHistory,0);
else if(activeTab==='git')setTimeout(function(){vscode.postMessage({type:'gitReady'})},0);
})();
</script>
</body>
</html>`;
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

export class FileExplorerController implements vscode.Disposable {
  private readonly model = new FileExplorerModel();
  private readonly propertiesProvider: FilePropertiesViewProvider;
  private readonly explorerProvider: FileExplorerViewProvider;
  private readonly readonlyScriptProvider = new ReadonlyExplorerScriptContentProvider();
  private readonly explorerSelectionEmitter = new vscode.EventEmitter<void>();
  public readonly onDidSelectExplorerNode = this.explorerSelectionEmitter.event;
  private readonly disposables: vscode.Disposable[] = [];
  private readonly pendingSettingsRefreshes = new Set<string>();
  private readonly propertyOnlySettingsRefreshUntil = new Map<string, number>();
  private readonly visibleViewTypes = new Set<string>();
  private linkState: Record<string, string> = {};
  private settingsRefreshTimer: NodeJS.Timeout | undefined;
  private startupRestoreTimer: NodeJS.Timeout | undefined;
  private settingsRefreshInFlight = false;
  private settingsRefreshAgain = false;
  private reniumActivityVisible = false;
  private observedVisibleViewThisSession = false;

  /** Show a decoded .renium instance's properties in the Renium Properties view
   * — used by the double-click custom editor, which has no Properties pane. */
  public showRbsyncPropertiesReadonly(node: {
    name?: string;
    className?: string;
    settingsId?: string;
    properties?: Record<string, unknown>;
    attributes?: Record<string, unknown>;
    pathSegments?: string[];
  }): void {
    this.propertiesProvider.showReadonlyInstance(node);
  }

  public constructor(private readonly context: vscode.ExtensionContext, private readonly git?: GitViewActions) {
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
      resaveLink: (node) => this.resaveLink(node),
      relinkLink: (node) => this.relinkLink(node),
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
      vscode.commands.registerCommand("renium.fileExplorer.showGit", () => this.explorerProvider.showGit()),
      vscode.commands.registerCommand("renium.fileExplorer.refreshGit", (options?: { fetch?: boolean }) =>
        this.explorerProvider.refreshGit(options ?? {}),
      ),
      vscode.commands.registerCommand("renium.fileExplorer.openScript", (node?: FileExplorerNode) => this.openScript(node)),
      vscode.commands.registerCommand("renium.fileExplorer.addInstance", (node?: FileExplorerNode) => this.addInstance(node)),
      vscode.commands.registerCommand("renium.fileExplorer.deleteInstance", (node?: FileExplorerNode) => this.deleteInstance(node)),
      vscode.commands.registerCommand("renium.fileExplorer.desyncPackageLink", (node?: FileExplorerNode) => this.desyncPackageLink(node)),
      vscode.commands.registerCommand("renium.fileExplorer.copyPath", (node?: FileExplorerNode) => this.copyPath(node)),
      vscode.commands.registerCommand("renium.fileExplorer.importModel", (node?: FileExplorerNode) => this.importModel(node)),
      vscode.commands.registerCommand("renium.fileExplorer.exportModel", (node?: FileExplorerNode) => this.exportModel(node)),
      vscode.commands.registerCommand("renium.fileExplorer.refreshServices", (services?: string[]) =>
        this.explorerProvider.refreshServices(Array.isArray(services) ? services : []),
      ),
      vscode.commands.registerCommand("renium.fileExplorer.refreshPropertyChanges", (settingsFiles?: string[]) =>
        this.refreshPropertyChanges(Array.isArray(settingsFiles) ? settingsFiles : []),
      ),
      vscode.commands.registerCommand("renium.properties.showPackageNode", (payload?: PackagePropertiesPayload) =>
        this.propertiesProvider.showReadonlyPackage(payload),
      ),
      vscode.commands.registerCommand("renium.fileExplorer.setLinkState", (keys?: Record<string, string>) => {
        this.linkState = keys ?? {};
        this.explorerProvider.setLinkState(this.linkState);
      }),
    );
    this.startSettingsWatcher();
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
    for (const disposable of this.disposables) {
      disposable.dispose();
    }
    this.explorerSelectionEmitter.dispose();
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

  private tabInputUris(input: unknown): vscode.Uri[] {
    const candidate = input as { uri?: unknown; original?: unknown; modified?: unknown };
    const uris: vscode.Uri[] = [];
    for (const value of [candidate.uri, candidate.original, candidate.modified]) {
      if (value instanceof vscode.Uri && value.scheme === "file") {
        uris.push(value);
      }
    }
    return uris;
  }

  private async closeSourceTabs(sourcePaths: string[] | undefined): Promise<void> {
    const pathKeys = new Set(
      (sourcePaths ?? [])
        .map((sourcePath) => String(sourcePath || "").trim())
        .filter((sourcePath) => sourcePath.length > 0)
        .map((sourcePath) => this.normalizedFileKey(sourcePath)),
    );
    if (pathKeys.size === 0) {
      return;
    }
    const tabs: vscode.Tab[] = [];
    for (const group of vscode.window.tabGroups.all) {
      for (const tab of group.tabs) {
        if (this.tabInputUris(tab.input).some((uri) => pathKeys.has(this.normalizedFileKey(uri.fsPath)))) {
          tabs.push(tab);
        }
      }
    }
    if (tabs.length > 0) {
      await vscode.window.tabGroups.close(tabs, true);
    }
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
    const config = getExplorerConfig();
    const root = srcRoot(config);
    const watchers = [SETTINGS_FILE_NAME, LEGACY_SETTINGS_FILE_NAME].map((fileName) =>
      vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(root, `**/${fileName}`)),
    );
    const queue = (uri: vscode.Uri): void => {
      if (uri.scheme === "file") {
        this.queueSettingsRefresh(uri.fsPath);
      }
    };
    for (const watcher of watchers) {
      watcher.onDidCreate(queue);
      watcher.onDidChange(queue);
      watcher.onDidDelete(queue);
      this.disposables.push(watcher);
    }
  }

  private queueSettingsRefresh(settingsFile: string): void {
    const normalizedSettingsFile = this.normalizeSettingsRefreshPath(settingsFile);
    this.clearExpiredPropertyOnlyRefreshes();
    const propertyOnlyUntil = this.propertyOnlySettingsRefreshUntil.get(normalizedSettingsFile);
    if (propertyOnlyUntil !== undefined && propertyOnlyUntil > Date.now()) {
      return;
    }
    this.pendingSettingsRefreshes.add(normalizedSettingsFile);
    if (this.settingsRefreshTimer) {
      clearTimeout(this.settingsRefreshTimer);
    }
    this.settingsRefreshTimer = setTimeout(() => {
      this.settingsRefreshTimer = undefined;
      void this.flushSettingsRefreshes();
    }, 600);
  }

  private async flushSettingsRefreshes(): Promise<void> {
    if (this.settingsRefreshInFlight) {
      this.settingsRefreshAgain = true;
      return;
    }
    this.settingsRefreshInFlight = true;
    try {
      do {
        this.settingsRefreshAgain = false;
        this.clearExpiredPropertyOnlyRefreshes();
        const settingsFiles = Array.from(this.pendingSettingsRefreshes).filter((settingsFile) => {
          const propertyOnlyUntil = this.propertyOnlySettingsRefreshUntil.get(settingsFile);
          return propertyOnlyUntil === undefined || propertyOnlyUntil <= Date.now();
        });
        this.pendingSettingsRefreshes.clear();
        if (settingsFiles.length > 0) {
          await this.explorerProvider.refreshSettingsFiles(settingsFiles);
        }
      } while (this.settingsRefreshAgain || this.pendingSettingsRefreshes.size > 0);
    } finally {
      this.settingsRefreshInFlight = false;
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

  private nodeLinkPathKey(node: FileExplorerNode): string | undefined {
    const pathSegments = node.pathSegments.length > 0 ? node.pathSegments : [node.service, node.name];
    if (pathSegments.length < 2) {
      return undefined;
    }
    return `${pathSegments[0]}\u0001${pathSegments.slice(1).join("/")}`;
  }

  private linkPathKey(service: string, pathSegments: string[]): string | undefined {
    const segments = pathSegments.length > 0 ? pathSegments : [service];
    const normalized = segments[0] === service ? segments : [service, ...segments];
    if (normalized.length < 2) {
      return undefined;
    }
    return `${normalized[0]}\u0001${normalized.slice(1).join("/")}`;
  }

  private directReniumLinkTargetPath(node: FileExplorerNode): string[] | undefined {
    const key = this.nodeLinkPathKey(node);
    return key && this.linkState[key] === "linked" ? this.nodeTargetPath(node) : undefined;
  }

  private childPathUnder(parent: FileExplorerNode, childName: string): string[] {
    const parentPath = parent.kind === "service" ? [parent.service] : this.nodeTargetPath(parent);
    return parentPath.concat(childName);
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
    const oldKey = this.linkPathKey(oldService, oldPathSegments);
    const newKey = this.linkPathKey(newService, newPathSegments);
    if (oldKey && newKey && this.linkState[oldKey]) {
      this.linkState[newKey] = this.linkState[oldKey];
      delete this.linkState[oldKey];
      this.explorerProvider.setLinkState(this.linkState);
    }
  }

  private inheritedLinkState(node: FileExplorerNode): string | undefined {
    let current: FileExplorerNode | undefined = node;
    while (current && current.kind !== "service") {
      const key = this.nodeLinkPathKey(current);
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
      vscode.window.showWarningMessage(`Renium: source preview not found for ${loaded.name}.`);
      return;
    }
    if (!loaded.sourcePath || !fs.existsSync(loaded.sourcePath)) {
      vscode.window.showWarningMessage(`Renium: source file not found for ${loaded.name}.`);
      return;
    }
    const document = await vscode.workspace.openTextDocument(vscode.Uri.file(loaded.sourcePath));
    await vscode.window.showTextDocument(document, { preview: false });
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
      vscode.window.showErrorMessage(`Renium: failed to add instance. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async deleteInstance(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service") {
      return;
    }
    try {
      const loaded = await this.model.ensureLoaded(node);
      const manifestTarget = this.reniumManifestTarget(loaded);
      if (manifestTarget && this.hasSiblingPathCollision(manifestTarget.node, manifestTarget.pathSegments)) {
        vscode.window.showWarningMessage(
          `Renium: cannot safely delete ${loaded.name} because another sibling has the same linked target path (${manifestTarget.pathSegments.join(".")}). Rename one of them first.`,
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
      const removeResult = await this.model.removeInstance(loaded);
      await this.closeSourceTabs(removeResult.removedSourcePaths);
      void vscode.commands.executeCommand("renium.packages.refresh").then(
        () => undefined,
        () => undefined,
      );
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (/no matched instance|no matching instance|instance not found/i.test(message)) {
        return;
      }
      vscode.window.showErrorMessage(`Renium: failed to delete instance. ${message}`);
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
      vscode.window.showInformationMessage(`Renium: removed ${removed} PackageLink${removed === 1 ? "" : "s"}.`);
    } catch (error) {
      vscode.window.showErrorMessage(`Renium: failed to desync package. ${error instanceof Error ? error.message : String(error)}`);
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
      vscode.window.showWarningMessage("Renium: select an Explorer node to import into.");
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
      vscode.window.showWarningMessage("Renium: choose one or more .rbxm or .rbxmx files to import.");
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
      vscode.window.showInformationMessage(`Renium: imported ${summary} into ${target.name}.`);
    } catch (error) {
      vscode.window.showErrorMessage(`Renium: failed to import model. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async exportModel(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service") {
      vscode.window.showWarningMessage("Renium: select an Explorer instance to export.");
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
      vscode.window.showInformationMessage(`Renium: exported ${loaded.name} to ${finalOutputPath}.`);
    } catch (error) {
      vscode.window.showErrorMessage(`Renium: failed to export model. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private nodeTargetPath(node: FileExplorerNode): string[] {
    return node.pathSegments.length > 0 ? node.pathSegments.slice() : [node.service, node.name];
  }

  private reniumManifestTarget(node: FileExplorerNode): { node: FileExplorerNode; pathSegments: string[] } | undefined {
    let current: FileExplorerNode | undefined = node;
    while (current && current.kind !== "service") {
      const key = this.nodeLinkPathKey(current);
      if (key && this.linkState[key]) {
        return { node: current, pathSegments: this.nodeTargetPath(current) };
      }
      if (current.hasPackageLink === true) {
        return { node: current, pathSegments: this.nodeTargetPath(current) };
      }
      if (current.className === "PackageLink") {
        const parent = current.parentTreeId ? this.model.getNode(current.parentTreeId) : undefined;
        if (parent && parent.kind !== "service") {
          return { node: parent, pathSegments: this.nodeTargetPath(parent) };
        }
        const pathSegments = this.nodeTargetPath(current);
        return pathSegments.length > 1 ? { node: current, pathSegments: pathSegments.slice(0, -1) } : undefined;
      }
      current = current.parentTreeId ? this.model.getNode(current.parentTreeId) : undefined;
    }
    return undefined;
  }

  private hasSiblingPathCollision(targetNode: FileExplorerNode, pathSegments: string[]): boolean {
    const parent = targetNode.parentTreeId ? this.model.getNode(targetNode.parentTreeId) : undefined;
    const targetKey = this.linkPathKey(targetNode.service, pathSegments);
    if (!parent || !targetKey) {
      return false;
    }
    return this.model.getChildren(parent).some((child) =>
      child.treeId !== targetNode.treeId &&
      this.linkPathKey(child.service, this.nodeTargetPath(child)) === targetKey
    );
  }

  private async createLink(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service") {
      vscode.window.showWarningMessage("Renium: select an Explorer instance to link.");
      return;
    }
    const loaded = await this.model.ensureLoaded(node);
    await vscode.commands.executeCommand("renium.link.packInstance", {
      service: loaded.service,
      pathSegments: this.nodeTargetPath(loaded),
    });
  }

  private async resaveLink(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service") {
      vscode.window.showWarningMessage("Renium: select a linked package root to resave.");
      return;
    }
    const loaded = await this.model.ensureLoaded(node);
    const target = this.reniumManifestTarget(loaded);
    if (!target) {
      vscode.window.showWarningMessage("Renium: select a linked package root to resave.");
      return;
    }
    await vscode.commands.executeCommand("renium.link.resavePackage", {
      service: loaded.service,
      pathSegments: target.pathSegments,
    });
  }

  private async relinkLink(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service") {
      vscode.window.showWarningMessage("Renium: select a broken package root to relink.");
      return;
    }
    const loaded = await this.model.ensureLoaded(node);
    const target = this.reniumManifestTarget(loaded);
    if (!target) {
      vscode.window.showWarningMessage("Renium: select a broken package root to relink.");
      return;
    }
    await vscode.commands.executeCommand("renium.link.relinkPackage", {
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
      vscode.window.showWarningMessage("Renium: drop the package onto an Explorer service or instance.");
      return;
    }
    const loaded = await this.model.ensureLoaded(node);
    if (loaded.hasChildren && loaded.children.length === 0) {
      await this.model.loadChildren(loaded, false).catch(() => undefined);
    }
    const parentPath = loaded.kind === "service"
      ? [loaded.service]
      : this.nodeTargetPath(loaded);
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
      vscode.window.showErrorMessage(`Renium: failed to insert package. ${message}`);
    }
  }

  private async breakLinkNode(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service") {
      return;
    }
    const loaded = await this.model.ensureLoaded(node);
    await vscode.commands.executeCommand("renium.link.breakInstance", {
      service: loaded.service,
      pathSegments: this.nodeTargetPath(loaded),
    });
  }
}
