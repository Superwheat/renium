import * as crypto from "crypto";
import * as fs from "fs";
import * as path from "path";
import { collectFilesRecursively, filesystemPathKey } from "./utils";

export type StudioSnapshotDiff = {
  changedServices: string[];
  fingerprintsByService: Map<string, string>;
};

const TRANSIENT_PROPERTY_NAMES = new Set([
  "absoluteposition",
  "absoluterotation",
  "absolutesize",
  "absolutecanvassize",
  "absolutewindowsize",
  "absolutecontentsize",
  "absolutecellcount",
  "absolutecellsize",
  "absolutepositionwrite",
  "absolutesizewrite",
  "arehingesdetected",
  "channelcount",
  "datamodelplaceversion",
  "floormaterial",
  "ispaused",
  "issmooth",
  "isspatial",
  "lastusedmodificationmethod",
  "localizedtext",
  "localizationmatchedsourcetext",
  "localizationmatchidentifier",
  "maxextents",
  "movedirection",
  "movedirectioninternal",
  "occupant",
  "opentypefeatureserror",
  "physicsreprrootpart",
  "rolloffgain",
  "rootpart",
  "seatpart",
  "steer",
  "terrain",
  "throttle",
  "timeposition",
  "timepositionreplicating",
  "timepositionreplicator",
  "resolution",
  "walkdirection",
  "weightcurrent",
  "weighttarget",
  "contenttext",
  "textbounds",
  "textfits",
  "assemblyangularvelocity",
  "assemblylinearvelocity",
  "assemblycenterofmass",
  "assemblymass",
  "assemblyrootpart",
  "centerofmass",
  "currentcamera",
  "currentphysicalproperties",
  "distributedgametime",
  "extentscframe",
  "extentssize",
  "isloaded",
  "isplaying",
  "mass",
  "networkissleeping",
  "playbackloudness",
  "receiveage",
  "rotvelocity",
  "timelength",
  "velocity",
]);

function comparePaths(leftPath: string, rightPath: string): number {
  const left = filesystemPathKey(leftPath);
  const right = filesystemPathKey(rightPath);
  return left < right ? -1 : left > right ? 1 : 0;
}

function stableJson(value: unknown): string {
  if (Array.isArray(value)) {
    return "[" + value.map(stableJson).join(",") + "]";
  }
  if (value && typeof value === "object") {
    const record = value as Record<string, unknown>;
    return "{" + Object.keys(record)
      .sort()
      .map((key) => JSON.stringify(key) + ":" + stableJson(record[key]))
      .join(",") + "}";
  }
  return JSON.stringify(value) ?? "null";
}

function instanceIndex(instance: Record<string, unknown>, fallbackIndex: number): number {
  const raw = instance.instanceIndex;
  if (typeof raw !== "number" || !Number.isFinite(raw)) {
    return fallbackIndex + 1;
  }
  const index = Math.floor(raw);
  return index > 0 ? index : fallbackIndex + 1;
}

function normalizeProperties(instance: Record<string, unknown>): boolean {
  const properties = instance.properties;
  if (!properties || typeof properties !== "object" || Array.isArray(properties)) {
    return false;
  }
  const source = properties as Record<string, unknown>;
  const transient = Object.keys(source).filter((key) => TRANSIENT_PROPERTY_NAMES.has(key.toLowerCase()));
  if (transient.length === 0) {
    return false;
  }
  const stable = { ...source };
  for (const key of transient) {
    delete stable[key];
  }
  instance.properties = stable;
  return true;
}

function normalizeInstances(snapshot: Record<string, unknown>, service: string): number | undefined {
  const rawInstances = snapshot.instances;
  if (!Array.isArray(rawInstances)) {
    return undefined;
  }
  const entries = rawInstances.map((entry) => (
    entry && typeof entry === "object" && !Array.isArray(entry)
      ? { ...(entry as Record<string, unknown>) }
      : entry
  ));
  const removedIndices = new Set<number>();
  let changed = false;
  for (let index = 0; index < entries.length; index += 1) {
    const entry = entries[index];
    if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
      continue;
    }
    const instance = entry as Record<string, unknown>;
    changed = normalizeProperties(instance) || changed;
    if (service === "Workspace" && index === 0) {
      const properties = instance.properties;
      if (properties && typeof properties === "object" && !Array.isArray(properties) && "CurrentCamera" in properties) {
        const stable = { ...(properties as Record<string, unknown>) };
        delete stable.CurrentCamera;
        instance.properties = stable;
        changed = true;
      }
    }
    if (instance.className === "Camera") {
      removedIndices.add(instanceIndex(instance, index));
      changed = true;
    }
  }
  const filtered = entries.filter((entry, index) => (
    !entry
      || typeof entry !== "object"
      || Array.isArray(entry)
      || !removedIndices.has(instanceIndex(entry as Record<string, unknown>, index))
  ));
  if (changed || filtered.length !== rawInstances.length) {
    snapshot.instances = filtered;
  }
  return filtered.length;
}

function normalizeRoot(content: Buffer, service: string): Buffer {
  const text = content.toString("utf8");
  try {
    const parsed = JSON.parse(text) as unknown;
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      const snapshot = parsed as Record<string, unknown>;
      const filteredInstanceCount = normalizeInstances(snapshot, service);
      const metadata = snapshot.metadata;
      if (metadata && typeof metadata === "object" && !Array.isArray(metadata)) {
        const stableMetadata = { ...(metadata as Record<string, unknown>) };
        delete stableMetadata.generatedAtUnix;
        if (filteredInstanceCount !== undefined) {
          stableMetadata.instanceCount = filteredInstanceCount;
        }
        snapshot.metadata = stableMetadata;
      }
      return Buffer.from(stableJson(snapshot), "utf8");
    }
  } catch {
  }
  return Buffer.from(
    text.replace(/("generatedAtUnix"\s*:\s*)-?\d+(\s*,?)/g, (_match, prefix, suffix) => prefix + "0" + suffix),
    "utf8",
  );
}

async function fingerprint(snapshotRoot: string, service: string): Promise<string | undefined> {
  const rootFile = path.join(snapshotRoot, service + ".json");
  const paths = [
    ...(fs.existsSync(rootFile) ? [rootFile] : []),
    ...await collectFilesRecursively(path.join(snapshotRoot, service)),
  ].sort(comparePaths);
  if (paths.length === 0) {
    return undefined;
  }
  const hash = crypto.createHash("sha256");
  let hashed = false;
  for (const filePath of paths) {
    let stat: fs.Stats;
    try {
      stat = await fs.promises.stat(filePath);
    } catch {
      continue;
    }
    if (!stat.isFile()) {
      continue;
    }
    const content = await fs.promises.readFile(filePath);
    const stableContent = filesystemPathKey(filePath) === filesystemPathKey(rootFile)
      ? normalizeRoot(content, service)
      : content;
    hash.update(filesystemPathKey(path.relative(snapshotRoot, filePath)));
    hash.update("\0");
    hash.update(String(stableContent.length));
    hash.update("\0");
    hash.update(stableContent);
    hash.update("\0");
    hashed = true;
  }
  return hashed ? hash.digest("hex") : undefined;
}

export async function diffStudioSnapshots(
  services: string[],
  snapshotRoot: string,
  previous: ReadonlyMap<string, string>,
): Promise<StudioSnapshotDiff> {
  const changedServices: string[] = [];
  const fingerprintsByService = new Map<string, string>();
  for (const service of services) {
    const current = await fingerprint(snapshotRoot, service);
    if (!current) {
      changedServices.push(service);
      continue;
    }
    fingerprintsByService.set(service, current);
    if (previous.get(service) !== current) {
      changedServices.push(service);
    }
  }
  return { changedServices, fingerprintsByService };
}

export function commitStudioSnapshotFingerprints(
  services: string[],
  fingerprints: ReadonlyMap<string, string> | undefined,
  target: Map<string, string>,
): void {
  if (!fingerprints) {
    return;
  }
  for (const service of services) {
    const fingerprint = fingerprints.get(service);
    if (fingerprint) {
      target.set(service, fingerprint);
    }
  }
}
