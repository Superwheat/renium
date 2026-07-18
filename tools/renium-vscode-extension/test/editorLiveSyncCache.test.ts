import assert from "node:assert/strict";
import { test } from "node:test";

import { changedEditorLiveSyncPaths } from "../src/editorLiveSyncCache";

test("missing hash cache sends every unique watcher event", () => {
  const changed = changedEditorLiveSyncPaths([
    { path: "src/A.luau", key: "src/a.luau", hash: "new-a" },
    { path: "src/A.luau", key: "src/a.luau", hash: "new-a" },
    { path: "src/Deleted.luau", key: "src/deleted.luau", hash: undefined },
  ], false, {});

  assert.deepEqual(changed, ["src/A.luau", "src/Deleted.luau"]);
});

test("existing hash cache ignores unchanged files and unknown deletions", () => {
  const changed = changedEditorLiveSyncPaths([
    { path: "src/Same.luau", key: "src/same.luau", hash: "same" },
    { path: "src/Changed.luau", key: "src/changed.luau", hash: "new" },
    { path: "src/Deleted.luau", key: "src/deleted.luau", hash: undefined },
    { path: "src/Unknown.luau", key: "src/unknown.luau", hash: undefined },
  ], true, {
    "src/same.luau": "same",
    "src/changed.luau": "old",
    "src/deleted.luau": "old",
  });

  assert.deepEqual(changed, ["src/Changed.luau", "src/Deleted.luau"]);
});
