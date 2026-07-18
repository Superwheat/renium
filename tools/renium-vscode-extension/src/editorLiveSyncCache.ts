export type EditorLiveSyncHashObservation = {
  path: string;
  key: string;
  hash: string | undefined;
};

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
