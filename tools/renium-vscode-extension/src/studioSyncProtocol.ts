export type StudioPropertyChange = {
  service?: string;
  settingsId?: string;
  className?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
  scope?: "metadata" | "property" | "attribute";
  property?: string;
  value?: unknown;
  seq?: number;
};

export type StudioChangeLog = {
  service?: string;
  settingsId?: string;
  action?: string;
  reason?: string;
  className?: string;
  path?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
  property?: string;
  attribute?: string;
  direct?: boolean;
  fullSync?: boolean;
  seq?: number;
};

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
  pullChanges?: boolean;
  paused?: boolean;
  pendingPaths?: string[];
  pushes?: number;
  pulls?: number;
  error?: string;
};

export type StudioChangeState = {
  ok?: boolean;
  tracking?: boolean;
  role?: string;
  seq?: number;
  runtimeId?: string;
  dirtyServices?: string[];
  fullSyncServices?: string[];
  propertyChanges?: StudioPropertyChange[];
  editorActions?: StudioEditorAction[];
  changes?: StudioChangeLog[];
  trackedServices?: number;
  itemChangedAvailable?: boolean;
  eventDriven?: boolean;
  waitSeconds?: number;
  waitTimedOut?: boolean;
  waitCancelled?: boolean;
  twoWaySyncEnabled?: boolean;
  runtimeSettingChanges?: Record<string, unknown>;
  runtimeSettingsSeq?: number;
  conflictResolution?: string;
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

function finiteNumberArray(value: unknown): number[] | undefined {
  return Array.isArray(value)
    ? value.map(Number).filter(Number.isFinite)
    : undefined;
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
  return Array.isArray(record.dirtyServices)
    || Array.isArray(record.fullSyncServices)
    || Array.isArray(record.propertyChanges)
    || Array.isArray(record.changes)
    || Array.isArray(record.editorActions)
    || typeof record.tracking === "boolean"
    || typeof record.seq === "number"
    || typeof record.trackedServices === "number"
    || typeof record.itemChangedAvailable === "boolean"
    || typeof record.eventDriven === "boolean"
    || typeof record.waitTimedOut === "boolean"
    || typeof record.waitCancelled === "boolean";
}

function studioChangeState(record: Record<string, unknown>): StudioChangeState {
  const daemon = recordValue(record.daemon);
  return {
    ok: typeof record.ok === "boolean" ? record.ok : undefined,
    tracking: typeof record.tracking === "boolean" ? record.tracking : undefined,
    role: typeof record.role === "string" ? record.role : undefined,
    seq: typeof record.seq === "number" ? record.seq : undefined,
    runtimeId: typeof record.runtimeId === "string" ? record.runtimeId : undefined,
    dirtyServices: stringArray(record.dirtyServices),
    fullSyncServices: stringArray(record.fullSyncServices),
    propertyChanges: objectArray(record.propertyChanges)?.map((value) => ({
      service: typeof value.service === "string" ? value.service : undefined,
      settingsId: typeof value.settingsId === "string" ? value.settingsId : undefined,
      className: typeof value.className === "string" ? value.className : undefined,
      pathSegments: stringArray(value.pathSegments),
      pathOrdinals: finiteNumberArray(value.pathOrdinals),
      scope: value.scope === "metadata" || value.scope === "attribute" ? value.scope : "property",
      property: typeof value.property === "string" ? value.property : undefined,
      value: value.value,
      seq: typeof value.seq === "number" ? value.seq : undefined,
    })),
    editorActions: objectArray(record.editorActions)?.map((value) => ({
      id: typeof value.id === "string" ? value.id : undefined,
      type: typeof value.type === "string" ? value.type : undefined,
      service: typeof value.service === "string" ? value.service : undefined,
      settingsId: typeof value.settingsId === "string" ? value.settingsId : undefined,
      pathSegments: stringArray(value.pathSegments),
      pathOrdinals: Array.isArray(value.pathOrdinals) ? value.pathOrdinals.map(Number) : undefined,
      version: typeof value.version === "string" ? value.version : undefined,
    })),
    changes: objectArray(record.changes)?.map((value) => ({
      service: typeof value.service === "string" ? value.service : undefined,
      settingsId: typeof value.settingsId === "string" ? value.settingsId : undefined,
      action: typeof value.action === "string" ? value.action : undefined,
      reason: typeof value.reason === "string" ? value.reason : undefined,
      className: typeof value.className === "string" ? value.className : undefined,
      path: typeof value.path === "string" ? value.path : undefined,
      pathSegments: stringArray(value.pathSegments),
      pathOrdinals: finiteNumberArray(value.pathOrdinals),
      property: typeof value.property === "string" ? value.property : undefined,
      attribute: typeof value.attribute === "string" ? value.attribute : undefined,
      direct: typeof value.direct === "boolean" ? value.direct : undefined,
      fullSync: typeof value.fullSync === "boolean" ? value.fullSync : undefined,
      seq: typeof value.seq === "number" ? value.seq : undefined,
    })),
    trackedServices: typeof record.trackedServices === "number" ? record.trackedServices : undefined,
    itemChangedAvailable: typeof record.itemChangedAvailable === "boolean" ? record.itemChangedAvailable : undefined,
    eventDriven: typeof record.eventDriven === "boolean" ? record.eventDriven : undefined,
    waitSeconds: typeof record.waitSeconds === "number" ? record.waitSeconds : undefined,
    waitTimedOut: typeof record.waitTimedOut === "boolean" ? record.waitTimedOut : undefined,
    waitCancelled: typeof record.waitCancelled === "boolean" ? record.waitCancelled : undefined,
    twoWaySyncEnabled: typeof record.twoWaySyncEnabled === "boolean" ? record.twoWaySyncEnabled : undefined,
    runtimeSettingChanges: recordValue(record.runtimeSettingChanges),
    runtimeSettingsSeq: typeof record.runtimeSettingsSeq === "number"
      ? record.runtimeSettingsSeq
      : undefined,
    conflictResolution: typeof record.conflictResolution === "string" ? record.conflictResolution : undefined,
    daemon: daemon ? {
      running: typeof daemon.running === "boolean" ? daemon.running : undefined,
      pullChanges: typeof daemon.pullChanges === "boolean" ? daemon.pullChanges : undefined,
      paused: typeof daemon.paused === "boolean" ? daemon.paused : undefined,
      pendingPaths: stringArray(daemon.pendingPaths),
      pushes: typeof daemon.pushes === "number" ? daemon.pushes : undefined,
      pulls: typeof daemon.pulls === "number" ? daemon.pulls : undefined,
      error: typeof daemon.error === "string" ? daemon.error : undefined,
    } : undefined,
  };
}

function parseStudioChangeStatePayload(payload: string): StudioChangeState | undefined {
  const record = parseObject(payload);
  if (!record) {
    return undefined;
  }
  const nested = recordValue(record.result);
  if (nested) {
    return studioChangeState(nested);
  }
  return looksLikeStudioChangeState(record) ? studioChangeState(record) : undefined;
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

export function studioChangeSeq(state: StudioChangeState): number | undefined {
  return typeof state.seq === "number" && Number.isFinite(state.seq)
    ? Math.max(0, Math.floor(state.seq))
    : undefined;
}

export function studioChangeAckOptions(
  observedSeq: number | undefined,
  runtimeId: string | undefined,
): { reset?: boolean; ackSeq?: number; runtimeId?: string; start?: boolean; suppressSeconds?: number } {
  return observedSeq === undefined
    ? { start: true }
    : { start: true, ackSeq: observedSeq, runtimeId };
}

export function studioChangeLogEntries(
  state: StudioChangeState | undefined,
  services?: readonly string[],
): StudioChangeLog[] {
  if (!state?.changes) {
    return [];
  }
  const serviceSet = services
    ? new Set(services.map((service) => service.trim()).filter(Boolean))
    : undefined;
  return state.changes
    .filter((change) => {
      const service = String(change.service ?? "").trim();
      return service.length > 0 && (!serviceSet || serviceSet.has(service));
    })
    .sort((left, right) => (Number(left.seq ?? 0) || 0) - (Number(right.seq ?? 0) || 0));
}
import { recordValue } from "./utils";
