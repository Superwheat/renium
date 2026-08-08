export type ExplorerSortableNode = {
  id: string;
  name: string;
  className: string;
};

export const DEFAULT_SYNC_SERVICES = [
  "Workspace",
  "Players",
  "Lighting",
  "MaterialService",
  "ReplicatedFirst",
  "ReplicatedStorage",
  "ServerScriptService",
  "ServerStorage",
  "StarterGui",
  "StarterPack",
  "StarterPlayer",
  "Teams",
  "SoundService",
  "VoiceChatService",
] as const;

const EXPLORER_SERVICE_ORDER = [
  ...DEFAULT_SYNC_SERVICES,
  "TextChatService",
  "TestService",
  "LocalizationService",
  "VRService",
] as const;

const EXPLORER_CLASS_RANK = [
  "PackageLink",
  "Camera",
  "Terrain",
  "Folder",
  "SpawnLocation",
] as const;

const explorerServiceOrder = new Map<string, number>(
  EXPLORER_SERVICE_ORDER.map((className, index) => [className, index] as const),
);

const explorerServiceNames = new Map<string, string>(
  EXPLORER_SERVICE_ORDER.map((className) => [className.toLowerCase(), className] as const),
);

const explorerClassRank = new Map<string, number>(
  EXPLORER_CLASS_RANK.map((className, index) => [className, index] as const),
);

function normalizedName(name: string): string {
  return name.toLowerCase();
}

export function canonicalExplorerServiceName(name: string): string {
  const trimmed = name.trim();
  return explorerServiceNames.get(trimmed.toLowerCase()) ?? trimmed;
}

function compareText(a: string, b: string): number {
  if (a < b) {
    return -1;
  }
  if (a > b) {
    return 1;
  }
  return 0;
}

export function compareExplorerNodes(a: ExplorerSortableNode, b: ExplorerSortableNode): number {
  const aServiceOrder = explorerServiceOrder.get(a.className);
  const bServiceOrder = explorerServiceOrder.get(b.className);
  if (aServiceOrder !== undefined && bServiceOrder !== undefined) {
    return aServiceOrder - bServiceOrder || compareText(normalizedName(a.name), normalizedName(b.name));
  }
  if (aServiceOrder !== undefined) {
    return -1;
  }
  if (bServiceOrder !== undefined) {
    return 1;
  }

  const aRank = explorerClassRank.get(a.className) ?? EXPLORER_CLASS_RANK.length;
  const bRank = explorerClassRank.get(b.className) ?? EXPLORER_CLASS_RANK.length;
  return aRank - bRank ||
    compareText(normalizedName(a.name), normalizedName(b.name)) ||
    compareText(a.className, b.className) ||
    compareText(a.id, b.id);
}
