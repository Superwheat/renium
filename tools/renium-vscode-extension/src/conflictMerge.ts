
export type ConflictSide = "ours" | "theirs";

export type ConflictPolicy = "prompt" | "filesystem" | "studio";

const MAX_LCS_CELLS = 4_000_000;

export interface MergeRegion {
  /** true when this region is a clean (auto-mergeable) span. */
  readonly clean: boolean;
  /** Resulting lines when clean. */
  readonly lines: string[];
  /** Present only when !clean. */
  readonly conflict?: {
    readonly base: string[];
    readonly ours: string[];
    readonly theirs: string[];
  };
}

export interface ThreeWayMergeResult {
  /** true when no region conflicted. */
  readonly clean: boolean;
  /** Ordered regions describing the merged document. */
  readonly regions: MergeRegion[];
  /** Number of conflicting regions. */
  readonly conflictCount: number;
}

/** Split text into lines. CRLF/CR are normalized to LF boundaries for comparison. */
export function splitLines(text: string): string[] {
  if (text.length === 0) {
    return [];
  }
  return text.split(/\r\n|\r|\n/);
}

export function joinLines(lines: string[], eol: "\n" | "\r\n" = "\n"): string {
  return lines.join(eol);
}

/**
 * Longest common subsequence of two line arrays.
 * Returns matched index pairs [aIndex, bIndex] in increasing order.
 * O(n*m) time/memory. Oversized comparisons deliberately return no anchors so
 * the caller resolves one whole-document conflict instead of exhausting memory.
 */
function lcsMatches(a: string[], b: string[]): Array<[number, number]> {
  const n = a.length;
  const m = b.length;
  if (n === 0 || m === 0) {
    return [];
  }
  if (n > Math.floor(MAX_LCS_CELLS / m)) {
    return [];
  }
  const dp: Uint32Array[] = new Array(n + 1);
  for (let i = 0; i <= n; i++) {
    dp[i] = new Uint32Array(m + 1);
  }
  for (let i = n - 1; i >= 0; i--) {
    const ai = a[i];
    const row = dp[i];
    const next = dp[i + 1];
    for (let j = m - 1; j >= 0; j--) {
      row[j] = ai === b[j] ? next[j + 1] + 1 : Math.max(next[j], row[j + 1]);
    }
  }
  const matches: Array<[number, number]> = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      matches.push([i, j]);
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      i++;
    } else {
      j++;
    }
  }
  return matches;
}

function arraysEqual(a: string[], b: string[]): boolean {
  if (a.length !== b.length) {
    return false;
  }
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) {
      return false;
    }
  }
  return true;
}

interface Anchor {
  readonly b: number;
  readonly o: number;
  readonly t: number;
}

/**
 * Three-way merge of `ours` and `theirs` against common ancestor `base`.
 * Produces ordered regions; a region changed by only one side (or by both
 * identically) is clean, a region changed differently by both is a conflict.
 */
export function threeWayMerge(base: string[], ours: string[], theirs: string[]): ThreeWayMergeResult {
  const mbOurs = new Map<number, number>();
  for (const [bi, oi] of lcsMatches(base, ours)) {
    mbOurs.set(bi, oi);
  }
  const mbTheirs = new Map<number, number>();
  for (const [bi, ti] of lcsMatches(base, theirs)) {
    mbTheirs.set(bi, ti);
  }

  const anchors: Anchor[] = [];
  for (let bi = 0; bi < base.length; bi++) {
    const o = mbOurs.get(bi);
    const t = mbTheirs.get(bi);
    if (o !== undefined && t !== undefined) {
      anchors.push({ b: bi, o, t });
    }
  }

  const regions: MergeRegion[] = [];
  let conflictCount = 0;

  const pushClean = (lines: string[]) => {
    if (lines.length > 0) {
      regions.push({ clean: true, lines });
    }
  };

  const emitSegment = (
    bLo: number, bHi: number,
    oLo: number, oHi: number,
    tLo: number, tHi: number,
  ) => {
    const baseSeg = base.slice(bLo, bHi);
    const ourSeg = ours.slice(oLo, oHi);
    const theirSeg = theirs.slice(tLo, tHi);
    if (baseSeg.length === 0 && ourSeg.length === 0 && theirSeg.length === 0) {
      return;
    }
    if (arraysEqual(ourSeg, theirSeg)) {
      pushClean(ourSeg);
      return;
    }
    if (arraysEqual(ourSeg, baseSeg)) {
      pushClean(theirSeg);
      return;
    }
    if (arraysEqual(theirSeg, baseSeg)) {
      pushClean(ourSeg);
      return;
    }
    conflictCount++;
    regions.push({
      clean: false,
      lines: [],
      conflict: { base: baseSeg, ours: ourSeg, theirs: theirSeg },
    });
  };

  let prevB = -1;
  let prevO = -1;
  let prevT = -1;
  for (const a of anchors) {
    emitSegment(prevB + 1, a.b, prevO + 1, a.o, prevT + 1, a.t);
    pushClean([base[a.b]]);
    prevB = a.b;
    prevO = a.o;
    prevT = a.t;
  }
  emitSegment(prevB + 1, base.length, prevO + 1, ours.length, prevT + 1, theirs.length);

  return { clean: conflictCount === 0, regions, conflictCount };
}

export interface RenderOptions {
  readonly oursLabel?: string;
  readonly theirsLabel?: string;
}

/**
 * Resolve a merge result to final text given a side preference for conflicts.
 * Clean regions always merge; conflicting regions take the preferred side.
 * Renium never writes git-style conflict markers into script files — overlapping
 * conflicts always resolve to one side (and both sides are backed up by the
 * caller), so the file on disk is always valid Lua.
 */
export function renderTakingSide(
  result: ThreeWayMergeResult,
  side: ConflictSide,
  eol: "\n" | "\r\n" = "\n",
): string {
  const lines: string[] = [];
  for (const region of result.regions) {
    if (region.clean) {
      lines.push(...region.lines);
      continue;
    }
    const c = region.conflict!;
    lines.push(...(side === "ours" ? c.ours : c.theirs));
  }
  return joinLines(lines, eol);
}

export interface ResolvedMerge {
  /** Final text to write (and/or push back). */
  readonly text: string;
  /** Whether any region conflicted. */
  readonly hadConflicts: boolean;
  /** True under the "prompt" policy when a conflict occurred: the local side was
   * kept and the caller should surface a diff so the user can review/switch. */
  readonly needsManualResolution: boolean;
  /** Number of conflicting regions. */
  readonly conflictCount: number;
}

/**
 * High-level entry point: merge three versions and resolve per policy.
 *   - "filesystem": overlapping conflicts resolved in favour of `ours`
 *   - "studio":     overlapping conflicts resolved in favour of `theirs`
 *   - "prompt":     keep `ours` (local) and set needsManualResolution so the
 *                   caller can surface a diff. No conflict markers are written.
 * Non-overlapping concurrent edits always merge automatically regardless.
 */
export function mergeAndResolve(
  baseText: string,
  oursText: string,
  theirsText: string,
  policy: ConflictPolicy,
  eol: "\n" | "\r\n" = "\n",
  options: RenderOptions = {},
): ResolvedMerge {
  const result = threeWayMerge(splitLines(baseText), splitLines(oursText), splitLines(theirsText));
  if (result.clean) {
    return {
      text: joinLines(result.regions.flatMap((r) => r.lines), eol),
      hadConflicts: false,
      needsManualResolution: false,
      conflictCount: 0,
    };
  }
  if (policy === "studio") {
    return { text: renderTakingSide(result, "theirs", eol), hadConflicts: true, needsManualResolution: false, conflictCount: result.conflictCount };
  }
  void options;
  return {
    text: renderTakingSide(result, "ours", eol),
    hadConflicts: true,
    needsManualResolution: policy === "prompt",
    conflictCount: result.conflictCount,
  };
}
