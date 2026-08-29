import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { test } from "node:test";

import {
  loadProjectSourceRoot,
  loadSharedConfig,
  sharedConfigValue,
} from "../src/sharedConfig";

function restoreEnvironment(name: "APPDATA" | "XDG_CONFIG_HOME", value: string | undefined): void {
  if (value === undefined) {
    delete process.env[name];
  } else {
    process.env[name] = value;
  }
}

test("shared config accepts comments and trailing commas without changing string contents", () => {
  const projectRoot = fs.mkdtempSync(path.join(os.tmpdir(), "renium-shared-config-"));
  const appData = process.env.APPDATA;
  const xdgConfigHome = process.env.XDG_CONFIG_HOME;
  process.env.APPDATA = path.join(projectRoot, "user-config");
  process.env.XDG_CONFIG_HOME = path.join(projectRoot, "user-config");
  fs.mkdirSync(path.join(projectRoot, ".git"));
  fs.writeFileSync(path.join(projectRoot, "renium.project.jsonc"), `{
    // Line comments are accepted.
    "settings": {
      "services": [
        "Workspace",
        "ReplicatedStorage", // So are trailing array commas.
      ],
      /* Block comments may span
         multiple lines. */
      "gitSync": {
        "commitMessageTemplate": "keep // and /* inside strings */",
      },
    },
  }\n`, "utf8");

  try {
    const config = loadSharedConfig(projectRoot, projectRoot);
    assert.deepEqual(sharedConfigValue(config, "services"), ["Workspace", "ReplicatedStorage"]);
    assert.equal(
      sharedConfigValue(config, "gitSync.commitMessageTemplate"),
      "keep // and /* inside strings */",
    );
  } finally {
    restoreEnvironment("APPDATA", appData);
    restoreEnvironment("XDG_CONFIG_HOME", xdgConfigHome);
    fs.rmSync(projectRoot, { recursive: true, force: true });
  }
});

test("shared config reports unterminated strings and block comments consistently", () => {
  for (const text of [
    "{\"sourceRoot\": \"src}",
    "{\"sourceRoot\": \"src\" /* comment}",
  ]) {
    const projectRoot = fs.mkdtempSync(path.join(os.tmpdir(), "renium-jsonc-error-"));
    fs.writeFileSync(path.join(projectRoot, "renium.project.jsonc"), text, "utf8");
    try {
      assert.throws(
        () => loadProjectSourceRoot(projectRoot),
        /Unterminated JSONC string or block comment/,
      );
    } finally {
      fs.rmSync(projectRoot, { recursive: true, force: true });
    }
  }
});
