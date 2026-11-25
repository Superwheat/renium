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

function restoreEnvironment(name: "APPDATA" | "USERPROFILE" | "XDG_CONFIG_HOME", value: string | undefined): void {
  if (value === undefined) {
    delete process.env[name];
  } else {
    process.env[name] = value;
  }
}

test("shared config accepts comments and trailing commas without changing string contents", () => {
  const projectRoot = fs.mkdtempSync(path.join(os.tmpdir(), "renium-shared-config-"));
  const appData = process.env.APPDATA;
  const userProfile = process.env.USERPROFILE;
  const xdgConfigHome = process.env.XDG_CONFIG_HOME;
  process.env.APPDATA = path.join(projectRoot, "user-config");
  process.env.USERPROFILE = path.join(projectRoot, "user-config");
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
    restoreEnvironment("USERPROFILE", userProfile);
    restoreEnvironment("XDG_CONFIG_HOME", xdgConfigHome);
    fs.rmSync(projectRoot, { recursive: true, force: true });
  }
});

test("shared config reads a project file that starts with a byte order mark", () => {
  const projectRoot = fs.mkdtempSync(path.join(os.tmpdir(), "renium-jsonc-bom-"));
  fs.writeFileSync(path.join(projectRoot, "renium.project.jsonc"), "\uFEFF{\"sourceRoot\": \"game\"}\n", "utf8");
  try {
    assert.equal(loadProjectSourceRoot(projectRoot), "game");
  } finally {
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

test("shared config accepts studioFpsMode in user config and rejects it in project config", () => {
  const projectRoot = fs.mkdtempSync(path.join(os.tmpdir(), "renium-fps-config-"));
  const appData = process.env.APPDATA;
  const userProfile = process.env.USERPROFILE;
  const xdgConfigHome = process.env.XDG_CONFIG_HOME;
  const userHome = path.join(projectRoot, "user-home");
  process.env.APPDATA = path.join(userHome, "AppData", "Roaming");
  process.env.USERPROFILE = userHome;
  process.env.XDG_CONFIG_HOME = path.join(userHome, ".config");
  fs.mkdirSync(path.join(projectRoot, ".git"));
  fs.mkdirSync(path.join(userHome, ".renium"), { recursive: true });
  fs.writeFileSync(path.join(userHome, ".renium", "config.json"), '{"studioFpsMode":"unlock"}\n', "utf8");
  fs.writeFileSync(path.join(projectRoot, "renium.project.jsonc"), '{"settings":{}}\n', "utf8");
  try {
    const config = loadSharedConfig(projectRoot, projectRoot);
    assert.equal(sharedConfigValue(config, "studioFpsMode"), "unlock");
    fs.writeFileSync(
      path.join(projectRoot, "renium.project.jsonc"),
      '{"settings":{"studioFpsMode":"unlock"}}\n',
      "utf8",
    );
    assert.throws(
      () => loadSharedConfig(projectRoot, projectRoot),
      /studioFpsMode is global/,
    );
  } finally {
    restoreEnvironment("APPDATA", appData);
    restoreEnvironment("USERPROFILE", userProfile);
    restoreEnvironment("XDG_CONFIG_HOME", xdgConfigHome);
    fs.rmSync(projectRoot, { recursive: true, force: true });
  }
});
