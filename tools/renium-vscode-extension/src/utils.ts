import * as vscode from "vscode";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";

export const SETTINGS_FILE_NAME = "__roblox_sync_settings.renium";

const PLACE_FILE_FORMATS = ["rbxl", "rbxlx"] as const;
const MODEL_FILE_FORMATS = ["rbxm", "rbxmx"] as const;

export type RobloxPlaceFormat = typeof PLACE_FILE_FORMATS[number];
export type RobloxModelFormat = typeof MODEL_FILE_FORMATS[number];

export function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export async function collectFilesRecursively(root: string): Promise<string[]> {
  const files: string[] = [];
  const stack = [root];
  while (stack.length > 0) {
    const directory = stack.pop();
    if (!directory) {
      continue;
    }
    let entries: fs.Dirent[];
    try {
      entries = await fs.promises.readdir(directory, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      const filePath = path.join(directory, entry.name);
      if (entry.isDirectory()) {
        stack.push(filePath);
      } else if (entry.isFile()) {
        files.push(filePath);
      }
    }
  }
  return files;
}

export function safeObject(value: unknown): Record<string, unknown> {
  return recordValue(value) ?? {};
}

export function recordValue(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined;
}

export function safeArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

export function prefixProcessOutput(prefix: string, data: Buffer | string): string {
  const lines = data.toString().replace(/\r\n/g, "\n").split("\n");
  if (lines.length === 1) {
    return `[${prefix}] ${lines[0]}`;
  }
  return lines
    .filter((line, index) => line.length > 0 || index < lines.length - 1)
    .map((line) => `[${prefix}] ${line}`)
    .join("\n") + "\n";
}

export function isScriptClass(className: unknown): boolean {
  return className === "Script" || className === "LocalScript" || className === "ModuleScript";
}

export function robloxScriptFileName(name: unknown, className: string | undefined): string {
  const safeName = String(name ?? "Script").replace(/[<>:"/\\|?*\x00-\x1f]/g, "_");
  if (/\.(lua|luau)$/i.test(safeName)) {
    return safeName;
  }
  if (className === "Script") {
    return `${safeName}.server.luau`;
  }
  if (className === "LocalScript") {
    return `${safeName}.client.luau`;
  }
  return `${safeName}.luau`;
}

function pickWorkspaceRootFolder(): vscode.WorkspaceFolder | undefined {
  const folders = vscode.workspace.workspaceFolders;
  if (!folders || folders.length === 0) {
    return undefined;
  }
  const activeUri = vscode.window.activeTextEditor?.document.uri;
  if (activeUri?.scheme === "file") {
    const activeFolder = vscode.workspace.getWorkspaceFolder(activeUri);
    if (activeFolder) {
      return activeFolder;
    }
  }
  if (folders.length > 1) {
    const match = folders.find((folder) => {
      const root = folder.uri.fsPath;
      return fs.existsSync(path.join(root, "renium.experience.json"))
        || fs.existsSync(path.join(root, "renium.project.json"))
        || fs.existsSync(path.join(root, "renium.project.jsonc"))
        || fs.existsSync(path.join(root, "src"))
        || fs.existsSync(path.join(root, "sourcemap.json"))
        || fs.existsSync(path.join(root, "renium-link.json"))
        || fs.existsSync(path.join(root, ".renium"));
    });
    if (match) {
      return match;
    }
  }
  return folders[0];
}

export function pickWorkspaceRoot(): string | undefined {
  return pickWorkspaceRootFolder()?.uri.fsPath;
}

export function isReniumSettingsFileName(fileName: string): boolean {
  return fileName.toLowerCase() === SETTINGS_FILE_NAME;
}

export function safeFileComponent(value: unknown): string {
  const cleaned = String(value ?? "item")
    .trim()
    .replace(/[^A-Za-z0-9._-]+/g, "_")
    .replace(/^_+|_+$/g, "")
    .slice(0, 80);
  return cleaned || "item";
}

function ensureRobloxFileExtension(filePath: string, format: string, formats: readonly string[]): string {
  const expected = `.${format}`;
  const current = path.extname(filePath).toLowerCase();
  if (current === expected) {
    return filePath;
  }
  if (formats.some((candidate) => current === `.${candidate}`)) {
    return `${filePath.slice(0, -current.length)}${expected}`;
  }
  return `${filePath}${expected}`;
}

function robloxFileFormatFromPath<T extends string>(filePath: string, formats: readonly T[]): T | undefined {
  const extension = path.extname(filePath).toLowerCase();
  return formats.find((format) => extension === `.${format}`);
}

export function ensurePlaceFileExtension(filePath: string, format: RobloxPlaceFormat): string {
  return ensureRobloxFileExtension(filePath, format, PLACE_FILE_FORMATS);
}

export function robloxPlaceFormatFromPath(filePath: string): RobloxPlaceFormat | undefined {
  return robloxFileFormatFromPath(filePath, PLACE_FILE_FORMATS);
}

export function ensureModelFileExtension(filePath: string, format: RobloxModelFormat): string {
  return ensureRobloxFileExtension(filePath, format, MODEL_FILE_FORMATS);
}

export function robloxModelFormatFromPath(filePath: string): RobloxModelFormat | undefined {
  return robloxFileFormatFromPath(filePath, MODEL_FILE_FORMATS);
}

export function resolveConfigPath(raw: string, root: string): string {
  const replaced = raw
    .replaceAll("${workspaceFolder}", root)
    .replaceAll("${userHome}", os.homedir());
  return path.isAbsolute(replaced) ? path.normalize(replaced) : path.normalize(path.join(root, replaced));
}

export function filesystemPathKey(filePath: string): string {
  const resolved = path.resolve(filePath);
  return process.platform === "win32" ? resolved.toLowerCase() : resolved;
}

export function isPathInside(filePath: string, rootPath: string): boolean {
  const relative = path.relative(filesystemPathKey(rootPath), filesystemPathKey(filePath));
  return relative === "" || (!!relative && !relative.startsWith("..") && !path.isAbsolute(relative));
}

export function compactCommandOutput(output: string, maxLines: number, maxChars: number): string {
  const text = output
    .replace(/\r\n/g, "\n")
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .slice(-maxLines)
    .join(" | ");
  return text.length <= maxChars ? text : `...${text.slice(-maxChars)}`;
}

export function ensureFileExists(filePath: string): void {
  if (!fs.existsSync(filePath)) {
    throw new Error(`Required file not found: ${filePath}`);
  }
}

export function isLuaSourcePath(filePath: string): boolean {
  return /\.(lua|luau)$/i.test(filePath);
}

export function normalizeServices(requested: readonly string[], fallback: readonly string[]): string[] {
  const services = new Set(requested.map((service) => service.trim()).filter(Boolean));
  if (services.size === 0) {
    fallback.forEach((service) => services.add(service));
  }
  return [...services];
}

export function normalizeReportedServices(
  reported: readonly string[],
  allowedServices: readonly string[],
): string[] {
  const allowed = new Set(allowedServices.map((service) => service.trim()).filter(Boolean));
  return [...new Set(reported.map((service) => String(service).trim()).filter((service) => allowed.has(service)))];
}

export function samePathSegments(left: readonly string[], right: readonly string[]): boolean {
  return left.length === right.length && left.every((segment, index) => segment === right[index]);
}

export function writeUtf8FileIfChanged(filePath: string, content: string): void {
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  const next = Buffer.from(content, "utf8");
  try {
    const current = fs.readFileSync(filePath);
    if (current.equals(next)) {
      return;
    }
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
      throw error;
    }
  }
  fs.writeFileSync(filePath, next);
}

export function tabInputUris(input: unknown, scheme?: string): vscode.Uri[] {
  const candidate = input as { uri?: unknown; original?: unknown; modified?: unknown };
  return [candidate.uri, candidate.original, candidate.modified].filter(
    (value): value is vscode.Uri => value instanceof vscode.Uri && (!scheme || value.scheme === scheme),
  );
}
