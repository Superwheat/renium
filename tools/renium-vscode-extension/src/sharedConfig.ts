import * as crypto from "crypto";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";

export type SharedConfig = Record<string, unknown>;

const cache = new Map<string, { fingerprint: string; value: SharedConfig }>();
const sourceRootCache = new Map<string, { fingerprint: string; value: string }>();
const scriptNamingCache = new Map<string, { fingerprint: string; value: ProjectScriptNaming }>();
const projectSourceGraphCache = new Map<string, ProjectSourceGraph>();
const STRING_CONFIG_KEYS = new Set([
  "projectRoot", "snapshotDir", "cliPath",
  "bridgePorts", "place", "daemon", "gitSync.gitPath", "gitSync.remote", "gitSync.branch",
  "gitSync.commitMessageTemplate", "wallySync.wallyPath",
  "wallySync.packagesDir", "wallySync.targetService", "wallySync.targetName",
  "wallySync.serverPackagesDir", "wallySync.serverTargetService", "wallySync.serverTargetName",
  "wallySync.devPackagesDir", "wallySync.devTargetService", "wallySync.devTargetName",
  "wallySync.realms",
  "link.manifest", "link.folder", "link.cacheDir", "link.gitPath",
]);
const BOOLEAN_CONFIG_KEYS = new Set([
  "yes", "backtrace", "verifyEditorPushSources", "adaptiveThrottle",
  "autoSyncOnSave", "editorLiveSyncEnabled",
  "studioLiveSyncEnabled", "liveSync.overridePackages", "runImport", "modifiedDefaultBypass",
  "gitSync.autoFetch", "gitSync.includeUntracked", "gitSync.confirmBeforePush",
  "gitSync.requireCleanWorktreeBeforePull", "wallySync.runInstall", "link.offline",
  "link.autoApplyOnManifestChange",
]);
const INTEGER_CONFIG_KEYS = new Set([
  "schemaVersion", "sourceWorkers", "instanceWorkers", "importWorkers", "chunkSize",
  "autoSyncDebounceMs", "studioLiveSyncPollMs",
  "liveSync.changesThreshold", "liveSync.diffLinesLimit",
]);
const NUMBER_CONFIG_KEYS = new Set([
  "bridgeWaitSeconds", "progressHeartbeatSeconds",
  "gitSync.timeoutSeconds",
]);
const STRING_ARRAY_CONFIG_KEYS = new Set(["services", "gitSync.stagePaths"]);
const ENUM_CONFIG_KEYS = new Map<string, readonly string[]>([
  ["importMode", ["direct", "snapshot"]],
  ["performanceMode", ["throughput", "balanced", "smooth"]],
  ["logLevel", ["off", "error", "warn", "info", "debug", "trace"]],
  ["color", ["auto", "always", "never"]],
  ["outputMode", ["text", "json", "pretty"]],
  ["liveSync.initialSyncPriority", ["studio", "editor", "none"]],
  ["liveSync.displayPrompts", ["always", "initial", "never"]],
  ["liveSync.conflictResolution", ["prompt", "filesystem", "studio"]],
  ["gitSync.pullFromStudioBeforePush", ["ask", "always", "never"]],
  ["gitSync.applyPulledChangesToStudio", ["ask", "always", "never"]],
  ["wallySync.applyToStudio", ["ask", "always", "never"]],
  ["link.applyToStudio", ["ask", "always", "never"]],
  ["gitSync.stageMode", ["tracked", "configuredPaths"]],
  ["gitSync.outputBehavior", ["onStart", "onError", "silent"]],
]);
const CONFIG_GROUP_KEYS = new Set(["gitSync", "wallySync", "liveSync", "link"]);

function fileContentFingerprint(filePath: string): string {
  try {
    const content = fs.readFileSync(filePath);
    return `sha256:${content.length}:${crypto.createHash("sha256").update(content).digest("hex")}`;
  } catch {
    return "-";
  }
}

export type ProjectSourceGraph = {
  roots: string[];
  locations: string[];
  files: string[];
  directories: string[];
  manifests: string[];
  ignored: string[];
  owners: ProjectSourceOwner[];
};

function cloneProjectSourceGraph(graph: ProjectSourceGraph): ProjectSourceGraph {
  return {
    roots: [...graph.roots],
    locations: [...graph.locations],
    files: [...graph.files],
    directories: [...graph.directories],
    manifests: [...graph.manifests],
    ignored: [...graph.ignored],
    owners: graph.owners.map((owner) => ({
      location: owner.location,
      target: [...owner.target],
    })),
  };
}

export type ProjectSourceOwner = {
  location: string;
  target: string[];
};

type ProjectScriptNaming = {
  extension: "preserve" | "luau" | "lua";
  serverSuffix: string;
  clientSuffix: string;
  moduleSuffix: string;
  pluginSuffix: string;
  clientRunContextSuffix: string;
};

type ProjectScriptIdentity = {
  className: "Script" | "LocalScript" | "ModuleScript";
  leafName?: string;
  runContext?: "Client" | "Plugin" | "Legacy";
};

function stripJsonc(text: string): string {
  let output = "";
  let inString = false;
  let escaped = false;
  let lineComment = false;
  let blockComment = false;
  for (let index = 0; index < text.length; index += 1) {
    const current = text[index];
    const next = text[index + 1];
    if (lineComment) {
      if (current === "\n" || current === "\r") {
        lineComment = false;
        output += current;
      }
      continue;
    }
    if (blockComment) {
      if (current === "*" && next === "/") {
        blockComment = false;
        index += 1;
      } else if (current === "\n" || current === "\r") {
        output += current;
      }
      continue;
    }
    if (inString) {
      output += current;
      if (escaped) {
        escaped = false;
      } else if (current === "\\") {
        escaped = true;
      } else if (current === "\"") {
        inString = false;
      }
      continue;
    }
    if (current === "\"") {
      inString = true;
      output += current;
    } else if (current === "/" && next === "/") {
      lineComment = true;
      index += 1;
    } else if (current === "/" && next === "*") {
      blockComment = true;
      index += 1;
    } else {
      output += current;
    }
  }
  if (inString || blockComment) {
    throw new Error("Unterminated JSONC string or block comment");
  }
  let withoutTrailing = "";
  inString = false;
  escaped = false;
  for (let index = 0; index < output.length; index += 1) {
    const current = output[index];
    if (inString) {
      withoutTrailing += current;
      if (escaped) {
        escaped = false;
      } else if (current === "\\") {
        escaped = true;
      } else if (current === "\"") {
        inString = false;
      }
      continue;
    }
    if (current === "\"") {
      inString = true;
      withoutTrailing += current;
      continue;
    }
    if (current === ",") {
      let next = index + 1;
      while (next < output.length && /\s/.test(output[next])) {
        next += 1;
      }
      if (output[next] === "}" || output[next] === "]") {
        continue;
      }
    }
    withoutTrailing += current;
  }
  return withoutTrailing;
}

function readObject(filePath: string): SharedConfig {
  if (!fs.existsSync(filePath)) {
    return {};
  }
  const text = fs.readFileSync(filePath, "utf8");
  const parsed = JSON.parse(stripJsonc(text)) as unknown;
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error(`${filePath} must contain a JSON object`);
  }
  return parsed as SharedConfig;
}

function isPortableRelativePath(value: string): boolean {
  if (!value || path.isAbsolute(value)) {
    return false;
  }
  const invalidNames = /^(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i;
  return value
    .split(/[\\/]/)
    .every((segment) =>
      segment.length > 0 &&
      segment !== ".." &&
      !/[<>:"|?*]/.test(segment) &&
      segment === segment.replace(/[ .]+$/, "") &&
      !invalidNames.test(segment),
    );
}

export function loadProjectSourceRoot(projectRoot: string): string {
  const filePath = projectFilePath(projectRoot);
  const fingerprint = fileContentFingerprint(filePath);
  const cached = sourceRootCache.get(filePath);
  if (cached?.fingerprint === fingerprint) {
    return cached.value;
  }
  const value = readObject(filePath).sourceRoot;
  if (value === undefined) {
    sourceRootCache.set(filePath, { fingerprint, value: "src" });
    return "src";
  }
  if (typeof value !== "string" || !isPortableRelativePath(value)) {
    throw new Error(`${filePath} sourceRoot must be a portable relative path`);
  }
  sourceRootCache.set(filePath, { fingerprint, value });
  return value;
}

export function loadProjectSourceLocations(projectRoot: string): string[] {
  return [...loadProjectSourceGraph(projectRoot).locations];
}

export function loadProjectSourceGraph(projectRoot: string): ProjectSourceGraph {
  const key = path.resolve(projectRoot);
  const cached = projectSourceGraphCache.get(key);
  if (cached) {
    return cloneProjectSourceGraph(cached);
  }
  const roots = new Set<string>();
  const locations = new Set<string>();
  const files = new Set<string>();
  const directories = new Set<string>();
  const manifests = new Set<string>();
  const ignored = new Set<string>();
  const owners = new Map<string, ProjectSourceOwner>();
  const visiting = new Set<string>();
  const targetSegments = (value: unknown): string[] | undefined => {
    if (typeof value === "string") {
      const segments = value.split(".");
      return segments.every((segment) => segment.length > 0) ? segments : undefined;
    }
    if (!value || typeof value !== "object" || Array.isArray(value)) {
      return undefined;
    }
    const segments = (value as SharedConfig).segments;
    if (
      !Array.isArray(segments)
      || segments.length === 0
      || !segments.every((segment) => typeof segment === "string" && segment.length > 0)
    ) {
      return undefined;
    }
    return [...segments] as string[];
  };
  const addOwner = (location: string, target: string[]): void => {
    const normalized = path.resolve(location);
    const ownerKey = `${normalized}\0${JSON.stringify(target)}`;
    owners.set(ownerKey, { location: normalized, target: [...target] });
  };
  const watchRoot = (location: string): string | undefined => {
    let candidate = location;
    try {
      const stat = fs.statSync(location);
      if (stat.isFile()) {
        candidate = path.dirname(location);
      }
    } catch {
      if (path.extname(location) !== "") {
        candidate = path.dirname(location);
      }
    }
    while (!fs.existsSync(candidate)) {
      const parent = path.dirname(candidate);
      if (parent === candidate) {
        return undefined;
      }
      candidate = parent;
    }
    return candidate;
  };
  const visitProject = (root: string, filePath: string, targetPrefix: string[]): void => {
    const key = path.resolve(filePath);
    if (visiting.has(key)) {
      throw new Error(`Nested project cycle includes ${key}`);
    }
    visiting.add(key);
    manifests.add(key);
    const project = readObject(key);
    const add = (
      value: unknown,
      target: string[],
      recurse: boolean,
      generated = false,
    ): void => {
      if (typeof value !== "string" || !isPortableRelativePath(value)) {
        return;
      }
      const resolved = path.resolve(root, value);
      if (generated) {
        ignored.add(resolved);
        return;
      }
      addOwner(resolved, target);
      locations.add(resolved);
      try {
        if (fs.statSync(resolved).isFile()) {
          files.add(resolved);
        } else {
          directories.add(resolved);
        }
      } catch {
        if (path.extname(resolved) !== "") {
          files.add(resolved);
        } else {
          directories.add(resolved);
        }
      }
      const watchBase = watchRoot(resolved);
      if (watchBase) {
        roots.add(watchBase);
      }
      const name = path.basename(resolved).toLowerCase();
      if (
        recurse &&
        fs.existsSync(resolved) &&
        fs.statSync(resolved).isFile() &&
        (name.endsWith(".project.json") || name.endsWith(".project.jsonc"))
      ) {
        visitProject(path.dirname(resolved), resolved, target);
      }
    };
    add(
      typeof project.sourceRoot === "string" ? project.sourceRoot : "src",
      targetPrefix,
      false,
    );
    const visitNode = (value: unknown, target: string[]): void => {
      if (!value || typeof value !== "object" || Array.isArray(value)) {
        return;
      }
      const node = value as SharedConfig;
      add(node.$path, target, true);
      for (const [name, child] of Object.entries(node)) {
        if (!name.startsWith("$")) {
          visitNode(child, [...target, name]);
        }
      }
    };
    if (project.tree && typeof project.tree === "object" && !Array.isArray(project.tree)) {
      for (const [name, node] of Object.entries(project.tree as SharedConfig)) {
        visitNode(node, [...targetPrefix, name]);
      }
    }
    if (Array.isArray(project.mounts)) {
      for (const value of project.mounts) {
        if (value && typeof value === "object" && !Array.isArray(value)) {
          const mount = value as SharedConfig;
          const target = targetSegments(mount.target);
          if (target) {
            add(mount.source, [...targetPrefix, ...target], true);
          }
        }
      }
    }
    if (Array.isArray(project.adapters)) {
      for (const value of project.adapters) {
        if (!value || typeof value !== "object" || Array.isArray(value)) {
          continue;
        }
        const adapter = value as SharedConfig;
        const direction = typeof adapter.direction === "string"
          ? adapter.direction.toLowerCase().replace(/[^a-z]/g, "")
          : "toproject";
        if (direction === "fromproject") {
          continue;
        }
        const target = targetSegments(adapter.target);
        if (target) {
          add(adapter.source, [...targetPrefix, ...target], true);
        }
        add(adapter.output, targetPrefix, false, true);
      }
    }
    visiting.delete(key);
  };
  visitProject(projectRoot, projectFilePath(projectRoot), []);
  const value = {
    roots: Array.from(roots).sort(),
    locations: Array.from(locations).sort(),
    files: Array.from(files).sort(),
    directories: Array.from(directories).sort(),
    manifests: Array.from(manifests).sort(),
    ignored: Array.from(ignored).sort(),
    owners: Array.from(owners.values()).sort((left, right) =>
      left.location.localeCompare(right.location)
      || JSON.stringify(left.target).localeCompare(JSON.stringify(right.target))),
  };
  projectSourceGraphCache.set(key, value);
  return cloneProjectSourceGraph(value);
}

export function invalidateProjectSourceGraph(projectRoot?: string): void {
  if (projectRoot === undefined) {
    projectSourceGraphCache.clear();
    sourceRootCache.clear();
    scriptNamingCache.clear();
    cache.clear();
    return;
  }
  const root = path.resolve(projectRoot);
  projectSourceGraphCache.delete(root);
  const insideRoot = (candidate: string): boolean => {
    const relative = path.relative(root, path.resolve(candidate));
    return relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative));
  };
  for (const key of sourceRootCache.keys()) {
    if (insideRoot(key)) {
      sourceRootCache.delete(key);
    }
  }
  for (const key of scriptNamingCache.keys()) {
    if (insideRoot(key)) {
      scriptNamingCache.delete(key);
    }
  }
  for (const key of cache.keys()) {
    if (insideRoot(key.split("\0").at(-1) ?? key)) {
      cache.delete(key);
    }
  }
}

function loadProjectScriptNaming(projectRoot: string): ProjectScriptNaming {
  const filePath = projectFilePath(projectRoot);
  const fingerprint = fileContentFingerprint(filePath);
  const cached = scriptNamingCache.get(filePath);
  if (cached?.fingerprint === fingerprint) {
    return { ...cached.value };
  }
  const project = readObject(filePath);
  const exportNaming = project.exportNaming;
  const names = exportNaming && typeof exportNaming === "object" && !Array.isArray(exportNaming)
    ? exportNaming as SharedConfig
    : {};
  const suffix = (key: string, fallback: string): string => {
    const value = names[key];
    return typeof value === "string" ? value : fallback;
  };
  const extension = project.scriptExtension === "lua" || project.scriptExtension === "luau"
    ? project.scriptExtension
    : "preserve";
  const value: ProjectScriptNaming = {
    extension,
    serverSuffix: suffix("serverSuffix", ".server"),
    clientSuffix: suffix("clientSuffix", ".client"),
    moduleSuffix: suffix("moduleSuffix", ""),
    pluginSuffix: suffix("pluginSuffix", ".plugin"),
    clientRunContextSuffix: suffix("clientRunContextSuffix", ".run-client"),
  };
  scriptNamingCache.set(filePath, { fingerprint, value });
  return { ...value };
}

export function inferProjectScriptIdentity(
  projectRoot: string,
  fileName: string,
): ProjectScriptIdentity | undefined {
  const naming = loadProjectScriptNaming(projectRoot);
  const extensions = naming.extension === "preserve" ? ["luau", "lua"] : [naming.extension];
  const patterns: Array<[string, ProjectScriptIdentity["className"], ProjectScriptIdentity["runContext"]]> = [
    [naming.clientRunContextSuffix, "Script", "Client"],
    [naming.pluginSuffix, "Script", "Plugin"],
    [naming.serverSuffix, "Script", "Legacy"],
    [naming.clientSuffix, "LocalScript", undefined],
    [naming.moduleSuffix, "ModuleScript", undefined],
  ];
  const candidates = extensions.flatMap((extension) =>
    patterns.map(([configuredSuffix, className, runContext]) => ({
      suffix: `${configuredSuffix}.${extension}`.toLowerCase(),
      className,
      runContext,
    })),
  ).sort((left, right) => right.suffix.length - left.suffix.length);
  const lower = fileName.toLowerCase();
  for (const candidate of candidates) {
    if (lower === `init${candidate.suffix}`) {
      return { className: candidate.className, runContext: candidate.runContext };
    }
    if (lower.endsWith(candidate.suffix) && fileName.length > candidate.suffix.length) {
      return {
        className: candidate.className,
        leafName: fileName.slice(0, fileName.length - candidate.suffix.length),
        runContext: candidate.runContext,
      };
    }
  }
  return undefined;
}

function merge(base: SharedConfig, overlay: SharedConfig): void {
  for (const [key, value] of Object.entries(overlay)) {
    if (value === null) {
      delete base[key];
      continue;
    }
    const existing = base[key];
    if (
      existing &&
      value &&
      typeof existing === "object" &&
      typeof value === "object" &&
      !Array.isArray(existing) &&
      !Array.isArray(value)
    ) {
      merge(existing as SharedConfig, value as SharedConfig);
    } else {
      base[key] = value;
    }
  }
}

function validateSharedConfig(config: SharedConfig): void {
  const visit = (value: SharedConfig, prefix: string): void => {
    for (const [key, item] of Object.entries(value)) {
      const dotted = prefix ? `${prefix}.${key}` : key;
      const enumValues = ENUM_CONFIG_KEYS.get(dotted);
      if (CONFIG_GROUP_KEYS.has(dotted)) {
        if (!item || typeof item !== "object" || Array.isArray(item)) {
          throw new Error(`Renium configuration key '${dotted}' must be an object`);
        }
        visit(item as SharedConfig, dotted);
      } else if (STRING_CONFIG_KEYS.has(dotted)) {
        if (typeof item !== "string") throw new Error(`Renium configuration key '${dotted}' must be a string`);
      } else if (BOOLEAN_CONFIG_KEYS.has(dotted)) {
        if (typeof item !== "boolean") throw new Error(`Renium configuration key '${dotted}' must be a boolean`);
      } else if (INTEGER_CONFIG_KEYS.has(dotted)) {
        if (!Number.isSafeInteger(item) || (dotted === "schemaVersion" && item !== 1)) {
          throw new Error(`Renium configuration key '${dotted}' must be ${dotted === "schemaVersion" ? "the integer 1" : "an integer"}`);
        }
      } else if (NUMBER_CONFIG_KEYS.has(dotted)) {
        if (typeof item !== "number" || !Number.isFinite(item)) {
          throw new Error(`Renium configuration key '${dotted}' must be a number`);
        }
      } else if (STRING_ARRAY_CONFIG_KEYS.has(dotted)) {
        if (!Array.isArray(item) || !item.every((entry) => typeof entry === "string")) {
          throw new Error(`Renium configuration key '${dotted}' must be an array of strings`);
        }
      } else if (enumValues) {
        if (typeof item !== "string" || !enumValues.includes(item)) {
          throw new Error(`Renium configuration key '${dotted}' has an unsupported value`);
        }
      } else {
        throw new Error(`Unknown Renium configuration key '${dotted}'`);
      }
    }
  };
  visit(config, "");
}

function findAncestor(start: string, markers: readonly string[]): string | undefined {
  let current = path.resolve(start);
  for (;;) {
    if (markers.some((marker) => fs.existsSync(path.join(current, marker)))) {
      return current;
    }
    const parent = path.dirname(current);
    if (parent === current) {
      return undefined;
    }
    current = parent;
  }
}

function projectFilePath(projectRoot: string): string {
  const jsonc = path.join(projectRoot, "renium.project.jsonc");
  const json = path.join(projectRoot, "renium.project.json");
  const hasJsonc = fs.existsSync(jsonc);
  const hasJson = fs.existsSync(json);
  if (hasJsonc && hasJson) {
    throw new Error(`${projectRoot} contains both renium.project.jsonc and renium.project.json`);
  }
  return hasJson ? json : jsonc;
}

function userConfigPath(): string {
  if (process.platform === "win32") {
    return path.join(process.env.APPDATA ?? path.join(os.homedir(), "AppData", "Roaming"), "Renium", "config.json");
  }
  if (process.platform === "darwin") {
    return path.join(os.homedir(), "Library", "Application Support", "Renium", "config.json");
  }
  return path.join(process.env.XDG_CONFIG_HOME ?? path.join(os.homedir(), ".config"), "renium", "config.json");
}

export function loadSharedConfig(workspaceRoot: string, projectRoot: string): SharedConfig {
  const workspace = findAncestor(projectRoot, [".git"]) ?? path.resolve(workspaceRoot);
  const experience = findAncestor(projectRoot, ["renium.experience.json"]) ?? path.resolve(projectRoot);
  const place = findAncestor(projectRoot, ["renium.project.jsonc", "renium.project.json"])
    ?? path.resolve(projectRoot);
  const projectFile = projectFilePath(place);
  const files = [
    userConfigPath(),
    path.join(workspace, ".renium", "workspace.config.json"),
    path.join(experience, "renium.config.json"),
    path.join(place, ".renium", "config.json"),
    projectFile,
  ];
  const key = `${workspace}\0${place}`;
  const fingerprint = files
    .map((filePath) => `${filePath}\0${fileContentFingerprint(filePath)}`)
    .join("\0");
  const cached = cache.get(key);
  if (cached?.fingerprint === fingerprint) {
    return cached.value;
  }
  const merged: SharedConfig = { schemaVersion: 1 };
  for (const filePath of files.slice(0, -1)) {
    merge(merged, readObject(filePath));
  }
  const project = readObject(files[files.length - 1]);
  const settings = project.settings;
  if (settings && typeof settings === "object" && !Array.isArray(settings)) {
    merge(merged, settings as SharedConfig);
  }
  validateSharedConfig(merged);
  cache.set(key, { fingerprint, value: merged });
  return merged;
}

export function sharedConfigValue<T>(config: SharedConfig, dottedKey: string): T | undefined {
  let value: unknown = config;
  for (const segment of dottedKey.split(".")) {
    if (!value || typeof value !== "object" || Array.isArray(value)) {
      return undefined;
    }
    value = (value as SharedConfig)[segment];
  }
  return value as T | undefined;
}
