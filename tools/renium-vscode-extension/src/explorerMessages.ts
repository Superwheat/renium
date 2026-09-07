import type { ExplorerViewMode } from "./explorerBackendClient";
import type { ReadonlyInstanceInfo } from "./fileExplorerCore";

type FieldValues = {
  text: string; number: number; boolean: boolean; texts: string[];
  mode: ExplorerViewMode; record: Record<string, unknown>; node: ReadonlyInstanceInfo;
};
const rows = { start: "number", count: "number", mode: "mode", revision: "number" } as const;
const node = { nodeId: "text" } as const;
const git = { projectRoot: "text", generation: "number" } as const;

// Runtime validation and the discriminated TypeScript contract use the same fields.
const messages = {
  storeDecode: { name: "text", base64: "text" }, storeDecodePath: { path: "text" },
  storeBrowse: {}, storeSelect: { node: "node" }, ready: {}, getRows: rows, prefetchRows: rows, refresh: {},
  gitReady: {}, gitRefresh: { ...git, fetch: "boolean" }, gitAction: { ...git, action: "text" },
  gitOpenOutput: {}, gitDiff: { ...git, path: "text" },
  packageDragDebug: { message: "text" }, cancelPackageDrag: {}, loadHistory: {},
  openHistoryBackup: { historyId: "text" }, compareHistoryBackup: { historyId: "text" },
  restoreHistory: { historyId: "text" }, restoreHistoryGroup: { historyIds: "texts", historyGroupId: "text" },
  searchLoad: { query: "text", revision: "number", count: "number" },
  clearSearch: { count: "number", revealId: "text" }, jumpMatch: { delta: "number", revision: "number" },
  expandNode: { ...node, ...rows }, collapseNode: { ...node, ...rows }, selectNode: node,
  openScript: node, addInstance: node, createInstance: { ...node, className: "text", name: "text" },
  renameInstance: { ...node, newName: "text" }, moveInstance: { ...node, targetId: "text" },
  deleteInstance: node, desyncPackageLink: node, copyInstance: node, pasteInstance: node, duplicateInstance: node,
  importModel: { ...node, modelPaths: "texts" }, exportModel: node, createLink: node, resaveLink: node,
  relinkLink: node, insertPackage: { ...node, linkId: "text", name: "text" }, breakLink: node, copyPath: node,
} as const satisfies Record<string, Record<string, keyof FieldValues>>;

export type ExplorerMessage = {
  [Type in keyof typeof messages]: { type: Type } & {
    [Field in keyof typeof messages[Type]]?: FieldValues[typeof messages[Type][Field] & keyof FieldValues]
  }
}[keyof typeof messages];

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function validField(kind: keyof FieldValues, value: unknown): boolean {
  switch (kind) {
    case "text": return typeof value === "string";
    case "number": return typeof value === "number" && Number.isFinite(value);
    case "boolean": return typeof value === "boolean";
    case "texts": return Array.isArray(value) && value.every(item => typeof item === "string");
    case "mode": return value === "normal" || value === "search";
    case "record": return isRecord(value);
    case "node": return isRecord(value) && validFields(value, {
      name: "text", className: "text", settingsId: "text", properties: "record", attributes: "record", pathSegments: "texts",
    });
  }
}

function validFields(value: Record<string, unknown>, fields: Record<string, keyof FieldValues>): boolean {
  return Object.entries(fields).every(([field, kind]) => value[field] === undefined || validField(kind, value[field]));
}

export function parseExplorerMessage(value: unknown): ExplorerMessage | undefined {
  if (!isRecord(value) || typeof value.type !== "string" || !Object.prototype.hasOwnProperty.call(messages, value.type)) {
    return undefined;
  }
  const fields = messages[value.type as keyof typeof messages];
  return validFields(value, fields) ? value as ExplorerMessage : undefined;
}
