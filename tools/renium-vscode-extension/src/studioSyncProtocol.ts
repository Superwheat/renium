import { recordValue } from "./utils";

export type StudioEditorAction = {
  id?: string;
  type?: string;
  service?: string;
  settingsId?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
  version?: string;
};

export type DaemonLiveSyncState = {
  running?: boolean;
  mode?: "reconcile" | "verify";
  paused?: boolean;
  pendingPaths?: string[];
  resolutionRequired?: boolean;
  error?: string;
};

export type StudioChangeState = {
  runtimeId?: string;
  editorActions?: StudioEditorAction[];
  editorActionCount?: number;
  twoWaySyncEnabled?: boolean;
  runtimeSettingChanges?: Record<string, unknown>;
  runtimeSettingChangeCount?: number;
  runtimeSettingsSeq?: number;
  daemon?: DaemonLiveSyncState;
};

function parseObject(value: string): Record<string, unknown> | undefined {
  if (!value) {
    return undefined;
  }
  try {
    return recordValue(JSON.parse(value) as unknown);
  } catch {
    return undefined;
  }
}

function objectArray(value: unknown): Record<string, unknown>[] | undefined {
  return Array.isArray(value)
    ? value.map(recordValue).filter((entry): entry is Record<string, unknown> => entry !== undefined)
    : undefined;
}

function stringArray(value: unknown): string[] | undefined {
  return Array.isArray(value) ? value.map(String) : undefined;
}

export function parseCliJsonObject<T extends object>(output: string): T | undefined {
  const lines = output.replace(/\r\n/g, "\n").split("\n");
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    const parsed = parseObject(lines[index].trim());
    if (parsed) {
      return parsed as T;
    }
  }
  return parseObject(output.trim()) as T | undefined;
}

export function parseEditorPushSummary(
  output: string,
  daemonResult?: unknown,
): Record<string, unknown> | undefined {
  const direct = recordValue(daemonResult);
  if (direct) {
    return direct;
  }
  const prefix = "__ROBLOX_SYNC_EDITOR_PUSH_RESULT__ ";
  let found: Record<string, unknown> | undefined;
  for (const rawLine of output.replace(/\r\n/g, "\n").split("\n")) {
    const line = rawLine.trim();
    const index = line.indexOf(prefix);
    if (index >= 0) {
      found = parseObject(line.slice(index + prefix.length)) ?? found;
    }
  }
  return found ?? parseObject(output.trim());
}

function looksLikeStudioChangeState(record: Record<string, unknown>): boolean {
  return recordValue(record.daemon) !== undefined
    || Array.isArray(record.editorActions)
    || typeof record.editorActionCount === "number"
    || recordValue(record.runtimeSettingChanges) !== undefined
    || typeof record.runtimeSettingChangeCount === "number"
    || typeof record.runtimeSettingsSeq === "number"
    || typeof record.twoWaySyncEnabled === "boolean";
}

function studioChangeState(record: Record<string, unknown>): StudioChangeState {
  const daemon = recordValue(record.daemon);
  return {
    runtimeId: typeof record.runtimeId === "string" ? record.runtimeId : undefined,
    editorActions: objectArray(record.editorActions)?.map((value) => ({
      id: typeof value.id === "string" ? value.id : undefined,
      type: typeof value.type === "string" ? value.type : undefined,
      service: typeof value.service === "string" ? value.service : undefined,
      settingsId: typeof value.settingsId === "string" ? value.settingsId : undefined,
      pathSegments: stringArray(value.pathSegments),
      pathOrdinals: Array.isArray(value.pathOrdinals) ? value.pathOrdinals.map(Number) : undefined,
      version: typeof value.version === "string" ? value.version : undefined,
    })),
    editorActionCount: typeof record.editorActionCount === "number"
      ? record.editorActionCount
      : undefined,
    twoWaySyncEnabled: typeof record.twoWaySyncEnabled === "boolean" ? record.twoWaySyncEnabled : undefined,
    runtimeSettingChanges: recordValue(record.runtimeSettingChanges),
    runtimeSettingChangeCount: typeof record.runtimeSettingChangeCount === "number"
      ? record.runtimeSettingChangeCount
      : undefined,
    runtimeSettingsSeq: typeof record.runtimeSettingsSeq === "number"
      ? record.runtimeSettingsSeq
      : undefined,
    daemon: daemon ? {
      running: typeof daemon.running === "boolean" ? daemon.running : undefined,
      mode: daemon.mode === "reconcile" || daemon.mode === "verify" ? daemon.mode : undefined,
      paused: typeof daemon.paused === "boolean" ? daemon.paused : undefined,
      pendingPaths: stringArray(daemon.pendingPaths),
      resolutionRequired: typeof daemon.resolutionRequired === "boolean"
        ? daemon.resolutionRequired
        : undefined,
      error: typeof daemon.error === "string" ? daemon.error : undefined,
    } : undefined,
  };
}

export function studioChangeStateFromValue(value: unknown): StudioChangeState | undefined {
  const record = recordValue(value);
  if (!record) {
    return undefined;
  }
  const nested = recordValue(record.result);
  if (nested) {
    return studioChangeState(nested);
  }
  return looksLikeStudioChangeState(record) ? studioChangeState(record) : undefined;
}

function parseStudioChangeStatePayload(payload: string): StudioChangeState | undefined {
  return studioChangeStateFromValue(parseObject(payload));
}

export function parseStudioChangeState(output: string): StudioChangeState | undefined {
  const prefix = "__ROBLOX_SYNC_STUDIO_CHANGE_STATE__ ";
  let found: StudioChangeState | undefined;
  for (const rawLine of output.replace(/\r\n/g, "\n").split("\n")) {
    const line = rawLine.trim();
    const index = line.indexOf(prefix);
    if (index >= 0) {
      found = parseStudioChangeStatePayload(line.slice(index + prefix.length)) ?? found;
    }
  }
  return found ?? parseStudioChangeStatePayload(output.trim());
}

export function summaryNumber(summary: Record<string, unknown>, key: string): number {
  const value = summary[key];
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}
