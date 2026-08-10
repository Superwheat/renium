import * as fs from "fs";
import * as path from "path";
import { filesystemPathKey } from "./utils";

const EXPERIENCE_FILE_NAME = "renium.experience.json";

export type ExperiencePlace = {
  placeId: number;
  name: string;
  root: string;
};

export type ExperienceManifest = {
  version: 2;
  gameId: number;
  startPlace: string;
  placeOrder: number[];
  places: Record<string, ExperiencePlace>;
};

type ActiveExperiencePlace = {
  alias: string;
  manifest: ExperienceManifest;
  place: ExperiencePlace;
  projectRoot: string;
  selector: string;
};

const activePlaces = new Map<string, string>();

function validInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

export function resolveExperiencePlaceRoot(projectRoot: string, placeRoot: string): string {
  if (!placeRoot || path.isAbsolute(placeRoot)) {
    throw new Error("Place roots must be non-empty paths relative to the experience root.");
  }
  const base = path.resolve(projectRoot);
  const resolved = path.resolve(base, placeRoot);
  const relative = path.relative(base, resolved);
  if (!relative || relative === ".." || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    throw new Error(`Place root must stay inside ${base}: ${placeRoot}`);
  }
  return resolved;
}

function normalizeAlias(name: string, placeId: number, preserveUnderscores: boolean): string {
  let normalized = "";
  let pendingSeparator = false;
  for (const character of name.toLowerCase()) {
    if (
      (character >= "a" && character <= "z") ||
      (character >= "0" && character <= "9")
    ) {
      if (pendingSeparator && normalized.length > 0) {
        normalized += "_";
      }
      normalized += character;
      pendingSeparator = false;
    } else if (/\s/.test(character) || (preserveUnderscores && character === "_")) {
      pendingSeparator = normalized.length > 0;
    }
  }
  return normalized || `place${placeId}`;
}

export function normalizePublishedPlaceName(name: string, placeId: number): string {
  return normalizeAlias(name, placeId, false);
}

export function normalizePlaceAlias(name: string, placeId: number): string {
  return normalizeAlias(name, placeId, true);
}

export function readExperienceManifest(projectRoot: string): ExperienceManifest | undefined {
  const manifestPath = path.join(projectRoot, EXPERIENCE_FILE_NAME);
  if (!fs.existsSync(manifestPath)) {
    return undefined;
  }
  let raw: unknown;
  try {
    raw = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  } catch (error) {
    throw new Error(`Could not read ${EXPERIENCE_FILE_NAME}: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    throw new Error(`${EXPERIENCE_FILE_NAME} must contain a JSON object.`);
  }
  const manifest = raw as Partial<Omit<ExperienceManifest, "version" | "placeOrder">> & {
    version?: unknown;
    placeOrder?: unknown;
  };
  if (manifest.version !== 1 && manifest.version !== 2) {
    throw new Error(`${EXPERIENCE_FILE_NAME} uses an unsupported version.`);
  }
  if (!validInteger(manifest.gameId)) {
    throw new Error(`${EXPERIENCE_FILE_NAME} gameId must be a non-negative integer.`);
  }
  if (!manifest.places || typeof manifest.places !== "object" || Array.isArray(manifest.places)) {
    throw new Error(`${EXPERIENCE_FILE_NAME} places must be an object.`);
  }
  const aliases = Object.keys(manifest.places);
  if (aliases.length === 0) {
    throw new Error(`${EXPERIENCE_FILE_NAME} must contain at least one place.`);
  }
  if (typeof manifest.startPlace !== "string" || !manifest.places[manifest.startPlace]) {
    throw new Error(`${EXPERIENCE_FILE_NAME} startPlace must name a configured place.`);
  }
  const publishedIds = new Set<number>();
  for (const alias of aliases) {
    if (!/^[a-z0-9]+(?:_[a-z0-9]+)*$/.test(alias)) {
      throw new Error(`Invalid place alias '${alias}' in ${EXPERIENCE_FILE_NAME}.`);
    }
    const place = manifest.places[alias];
    if (!place || typeof place !== "object" || Array.isArray(place)) {
      throw new Error(`Place '${alias}' must contain an object.`);
    }
    if (!validInteger(place.placeId)) {
      throw new Error(`Place '${alias}' has an invalid placeId.`);
    }
    if (place.placeId > 0 && !publishedIds.add(place.placeId)) {
      throw new Error(`Published placeId ${place.placeId} appears more than once.`);
    }
    if (typeof place.name !== "string" || !place.name.trim()) {
      throw new Error(`Place '${alias}' must have a name.`);
    }
    if (typeof place.root !== "string") {
      throw new Error(`Place '${alias}' must have a root.`);
    }
    resolveExperiencePlaceRoot(projectRoot, place.root);
  }
  if (manifest.placeOrder !== undefined && !Array.isArray(manifest.placeOrder)) {
    throw new Error(`${EXPERIENCE_FILE_NAME} placeOrder must be an array.`);
  }
  const placeOrder: number[] = [];
  const orderedIds = new Set<number>();
  if (manifest.version === 1) {
    for (const value of manifest.placeOrder ?? []) {
      if (typeof value !== "string" || !manifest.places[value]) {
        throw new Error(`${EXPERIENCE_FILE_NAME} placeOrder contains an unknown place alias.`);
      }
      const placeId = manifest.places[value].placeId;
      if (placeId > 0 && !orderedIds.has(placeId)) {
        orderedIds.add(placeId);
        placeOrder.push(placeId);
      }
    }
  } else {
    const configuredIds = new Set(
      aliases
        .map((alias) => manifest.places![alias].placeId)
        .filter((placeId) => placeId > 0),
    );
    for (const value of manifest.placeOrder ?? []) {
      if (!validInteger(value) || value === 0) {
        throw new Error(`${EXPERIENCE_FILE_NAME} placeOrder must contain positive place IDs.`);
      }
      if (!configuredIds.has(value)) {
        throw new Error(`${EXPERIENCE_FILE_NAME} placeOrder contains unknown placeId ${value}.`);
      }
      if (!orderedIds.has(value)) {
        orderedIds.add(value);
        placeOrder.push(value);
      }
    }
  }
  for (const alias of aliases) {
    const placeId = manifest.places[alias].placeId;
    if (placeId > 0 && !orderedIds.has(placeId)) {
      orderedIds.add(placeId);
      placeOrder.push(placeId);
    }
  }
  return {
    ...(manifest as Omit<ExperienceManifest, "version" | "placeOrder">),
    version: 2,
    placeOrder,
  };
}

export function experiencePlaceAliasesInOrder(manifest: ExperienceManifest): string[] {
  const aliasByPlaceId = new Map<number, string>();
  for (const [alias, place] of Object.entries(manifest.places)) {
    if (place.placeId > 0) {
      aliasByPlaceId.set(place.placeId, alias);
    }
  }
  const aliases: string[] = [];
  const seen = new Set<string>();
  for (const placeId of manifest.placeOrder) {
    const alias = aliasByPlaceId.get(placeId);
    if (alias && !seen.has(alias)) {
      seen.add(alias);
      aliases.push(alias);
    }
  }
  for (const alias of Object.keys(manifest.places)) {
    if (!seen.has(alias)) {
      seen.add(alias);
      aliases.push(alias);
    }
  }
  return aliases;
}

export function writeExperienceManifest(projectRoot: string, manifest: ExperienceManifest): void {
  fs.mkdirSync(projectRoot, { recursive: true });
  const manifestPath = path.join(projectRoot, EXPERIENCE_FILE_NAME);
  const temporary = `${manifestPath}.tmp-${process.pid}-${Date.now()}`;
  try {
    fs.writeFileSync(temporary, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
    fs.renameSync(temporary, manifestPath);
  } finally {
    if (fs.existsSync(temporary)) {
      fs.unlinkSync(temporary);
    }
  }
}

export function setActiveExperiencePlace(projectRoot: string, alias: string | undefined): void {
  const key = filesystemPathKey(projectRoot);
  if (alias) {
    activePlaces.set(key, alias);
  } else {
    activePlaces.delete(key);
  }
}

export function activeExperienceAlias(projectRoot: string): string | undefined {
  return activePlaces.get(filesystemPathKey(projectRoot));
}

export function resolveActiveExperiencePlace(projectRoot: string): ActiveExperiencePlace | undefined {
  const manifest = readExperienceManifest(projectRoot);
  if (!manifest) {
    return undefined;
  }
  const selected = activeExperienceAlias(projectRoot);
  const alias = selected && manifest.places[selected] ? selected : manifest.startPlace;
  return resolveExperiencePlaceByAlias(projectRoot, alias, manifest);
}

export function resolveExperiencePlaceByAlias(
  projectRoot: string,
  alias: string,
  manifest = readExperienceManifest(projectRoot),
): ActiveExperiencePlace {
  if (!manifest || !manifest.places[alias]) {
    throw new Error(`Place alias '${alias}' is not configured.`);
  }
  const place = manifest.places[alias];
  const selector = manifest.gameId > 0 && place.placeId > 0
    ? `${manifest.gameId}:${place.placeId}`
    : place.placeId > 0
      ? String(place.placeId)
      : place.name;
  return {
    alias,
    manifest,
    place,
    projectRoot: resolveExperiencePlaceRoot(projectRoot, place.root),
    selector,
  };
}

export function uniquePlaceAlias(
  projectRoot: string,
  manifest: ExperienceManifest | undefined,
  name: string,
  placeId: number,
): string {
  const base = normalizePublishedPlaceName(name, placeId);
  const available = (candidate: string): boolean =>
    !manifest?.places[candidate] &&
    !fs.existsSync(path.join(projectRoot, "places", candidate));
  if (available(base)) {
    return base;
  }
  const withId = `${base}_${placeId}`;
  if (available(withId)) {
    return withId;
  }
  let suffix = 2;
  while (!available(`${withId}_${suffix}`)) {
    suffix += 1;
  }
  return `${withId}_${suffix}`;
}
