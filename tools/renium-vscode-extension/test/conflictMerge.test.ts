import assert from "node:assert/strict";
import { test } from "node:test";

import { mergeAndResolve, sameSourceText, threeWayMerge, withLineEnding } from "../src/conflictMerge";

test("mergeAndResolve combines non-overlapping edits from both sides", () => {
  const result = mergeAndResolve("a\nb\nc", "A\nb\nc", "a\nb\nC", "prompt");

  assert.equal(result.text, "A\nb\nC");
  assert.equal(result.hadConflicts, false);
  assert.equal(result.needsManualResolution, false);
});

test("mergeAndResolve resolves overlapping edits without conflict markers", () => {
  const filesystem = mergeAndResolve("base", "local", "studio", "filesystem");
  const studio = mergeAndResolve("base", "local", "studio", "studio");
  const prompt = mergeAndResolve("base", "local", "studio", "prompt");

  assert.equal(filesystem.text, "local");
  assert.equal(studio.text, "studio");
  assert.equal(prompt.text, "local");
  assert.equal(prompt.needsManualResolution, true);
  for (const result of [filesystem, studio, prompt]) {
    assert.equal(result.hadConflicts, true);
    assert.equal(result.text.includes("<<<<<<<"), false);
  }
});

test("threeWayMerge falls back to one conflict for oversized comparisons", () => {
  const base = Array.from({ length: 2100 }, (_, index) => `line-${index}`);
  const ours = [...base];
  const theirs = [...base];
  ours[0] = "local-change";
  theirs[theirs.length - 1] = "studio-change";

  const result = threeWayMerge(base, ours, theirs);

  assert.equal(result.clean, false);
  assert.equal(result.conflictCount, 1);
  assert.equal(result.regions.length, 1);
});

test("source comparison ignores line-ending-only differences", () => {
  assert.equal(sameSourceText("a\r\nb\r\n", "a\nb\n"), true);
  assert.equal(sameSourceText("a\rb", "a\nb"), true);
  assert.equal(sameSourceText("a\nb", "a\nc"), false);
});

test("accepted Studio source keeps the filesystem EOL convention", () => {
  assert.equal(withLineEnding("a\nb\n", "\r\n"), "a\r\nb\r\n");
  assert.equal(withLineEnding("a\r\nb\r\n", "\n"), "a\nb\n");
});
