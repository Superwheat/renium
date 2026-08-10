import * as crypto from "crypto";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";

type EditorLiveSyncHashObservation = {
  path: string;
  key: string;
  hash: string | undefined;
};

type EditorLiveSyncHashCache = {
  version: number;
  projectRoot: string;
  updatedAtUnixMs: number;
  files: Record<string, string>;
};

function editorLiveSyncCachePath(projectRoot: string): string {
  return path.join(projectRoot, ".renium", "editor-live-sync-cache.json");
}

export function emptyEditorLiveSyncCache(projectRoot: string): EditorLiveSyncHashCache {
  return {
    version: 1,
    projectRoot: path.resolve(projectRoot),
    updatedAtUnixMs: Date.now(),
    files: {},
  };
}

export function loadEditorLiveSyncCache(
  projectRoot: string,
): { cache: EditorLiveSyncHashCache; existed: boolean } {
  try {
    const parsed = JSON.parse(
      fs.readFileSync(editorLiveSyncCachePath(projectRoot), "utf8"),
    ) as Partial<EditorLiveSyncHashCache>;
    if (
      parsed.version === 1 &&
      parsed.files &&
      typeof parsed.files === "object" &&
      !Array.isArray(parsed.files)
    ) {
      return {
        existed: true,
        cache: {
          version: 1,
          projectRoot: typeof parsed.projectRoot === "string" ? parsed.projectRoot : path.resolve(projectRoot),
          updatedAtUnixMs: typeof parsed.updatedAtUnixMs === "number" ? parsed.updatedAtUnixMs : 0,
          files: Object.fromEntries(
            Object.entries(parsed.files).filter((entry): entry is [string, string] => typeof entry[1] === "string"),
          ),
        },
      };
    }
  } catch {
  }
  return { existed: false, cache: emptyEditorLiveSyncCache(projectRoot) };
}

export function saveEditorLiveSyncCache(projectRoot: string, cache: EditorLiveSyncHashCache): void {
  const cachePath = editorLiveSyncCachePath(projectRoot);
  fs.mkdirSync(path.dirname(cachePath), { recursive: true });
  fs.writeFileSync(cachePath, `${JSON.stringify({
    version: 1,
    projectRoot: path.resolve(projectRoot),
    updatedAtUnixMs: Date.now(),
    files: cache.files,
  }, null, 2)}${os.EOL}`, "utf8");
}

export function editorLiveSyncCacheKey(filePath: string, projectRoot: string): string {
  const relative = path.relative(projectRoot, path.resolve(projectRoot, filePath));
  const normalized = relative.split(path.sep).join("/");
  return process.platform === "win32" ? normalized.toLowerCase() : normalized;
}

export async function editorLiveSyncFileHash(filePath: string): Promise<string | undefined> {
  try {
    const stat = await fs.promises.stat(filePath);
    if (!stat.isFile()) {
      return undefined;
    }
    const hash = crypto.createHash("sha256");
    hash.update(await fs.promises.readFile(filePath));
    return `sha256:${stat.size}:${hash.digest("hex")}`;
  } catch {
    return undefined;
  }
}

export function changedEditorLiveSyncPaths(
  observations: EditorLiveSyncHashObservation[],
  cacheExisted: boolean,
  cachedHashes: Record<string, string>,
): string[] {
  const seen = new Set<string>();
  const changed: string[] = [];

  for (const observation of observations) {
    if (seen.has(observation.key)) {
      continue;
    }
    seen.add(observation.key);
    if (!cacheExisted) {
      changed.push(observation.path);
      continue;
    }
    if (observation.hash === undefined) {
      if (cachedHashes[observation.key] !== undefined) {
        changed.push(observation.path);
      }
      continue;
    }
    if (cachedHashes[observation.key] !== observation.hash) {
      changed.push(observation.path);
    }
  }

  return changed;
}
