import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { test } from "node:test";

import {
  bundledReniumCliPath,
  findExecutableOnPath,
  reniumCliCandidates,
} from "../src/cliResolution";

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

test("reniumCliCandidates prefers an explicit path and the bundled CLI before PATH", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "renium-candidates-"));
  const extensionRoot = path.join(root, "extension");
  const pathRoot = path.join(root, "path");
  const explicit = path.join(root, "custom", process.platform === "win32" ? "renium.exe" : "renium");
  const pathBinary = path.join(pathRoot, process.platform === "win32" ? "renium.exe" : "renium");
  fs.mkdirSync(pathRoot, { recursive: true });
  fs.writeFileSync(pathBinary, "binary");
  if (process.platform !== "win32") {
    fs.chmodSync(pathBinary, 0o755);
  }

  try {
    const candidates = reniumCliCandidates({
      configuredPath: explicit,
      extensionRoot,
      pathValue: pathRoot,
      platform: process.platform,
      arch: process.arch,
      pathExtValue: process.platform === "win32" ? ".EXE" : undefined,
    });
    assert.equal(candidates[0], path.normalize(explicit));
    assert.equal(candidates[1], bundledReniumCliPath(extensionRoot));
    assert.equal(
      process.platform === "win32" ? candidates[2].toLowerCase() : candidates[2],
      process.platform === "win32" ? pathBinary.toLowerCase() : pathBinary,
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
