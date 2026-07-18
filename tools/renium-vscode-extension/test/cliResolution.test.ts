import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { test } from "node:test";

import { findExecutableOnPath } from "../src/cliResolution";

test("findExecutableOnPath finds Renium in PATH", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "renium-path-"));
  const binary = path.join(root, process.platform === "win32" ? "renium.exe" : "renium");
  fs.writeFileSync(binary, "binary");
  if (process.platform !== "win32") {
    fs.chmodSync(binary, 0o755);
  }

  try {
    const result = findExecutableOnPath(
      "renium",
      root,
      process.platform,
      process.platform === "win32" ? ".EXE" : undefined,
    );
    assert.equal(
      process.platform === "win32" ? result?.toLowerCase() : result,
      process.platform === "win32" ? binary.toLowerCase() : binary,
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("findExecutableOnPath returns undefined when no candidate exists", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "renium-path-empty-"));
  try {
    assert.equal(findExecutableOnPath("renium", root, process.platform, ".EXE"), undefined);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
