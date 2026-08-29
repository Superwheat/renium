import assert from "node:assert/strict";
import * as fs from "node:fs";
import Module from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import { test } from "node:test";

const moduleLoader = Module as unknown as {
  _load(request: string, parent: NodeModule | null, isMain: boolean): unknown;
};
const originalLoad = moduleLoader._load;
moduleLoader._load = (request, parent, isMain) =>
  request === "vscode" ? {} : originalLoad.call(moduleLoader, request, parent, isMain);
const { readExperienceManifest } = require("../src/experience") as typeof import("../src/experience");
moduleLoader._load = originalLoad;

function withManifest(manifest: unknown, run: (projectRoot: string) => void): void {
  const projectRoot = fs.mkdtempSync(path.join(os.tmpdir(), "renium-experience-"));
  fs.writeFileSync(
    path.join(projectRoot, "renium.experience.json"),
    JSON.stringify(manifest),
    "utf8",
  );
  try {
    run(projectRoot);
  } finally {
    fs.rmSync(projectRoot, { recursive: true, force: true });
  }
}

test("readExperienceManifest preserves legacy ordering while upgrading to version 2", () => {
  const places = {
    draft: { placeId: 0, name: "Draft", root: "places/draft" },
    lobby: { placeId: 101, name: "Lobby", root: "places/lobby" },
    battle: { placeId: 202, name: "Battle", root: "places/battle" },
    results: { placeId: 303, name: "Results", root: "places/results" },
  };
  const manifest = {
    version: 1,
    gameId: 99,
    startPlace: "draft",
    placeOrder: ["battle", "draft", "battle"],
    places,
    metadata: { preserved: true },
  };

  withManifest(manifest, (projectRoot) => {
    assert.deepEqual(readExperienceManifest(projectRoot), {
      ...manifest,
      version: 2,
      placeOrder: [202, 101, 303],
    });
  });
});

test("readExperienceManifest deduplicates version 2 ordering before appending configured places", () => {
  const places = {
    first: { placeId: 11, name: "First", root: "places/first" },
    second: { placeId: 22, name: "Second", root: "places/second" },
    third: { placeId: 33, name: "Third", root: "places/third" },
  };

  withManifest({
    version: 2,
    gameId: 99,
    startPlace: "first",
    placeOrder: [22, 22],
    places,
  }, (projectRoot) => {
    assert.deepEqual(readExperienceManifest(projectRoot)?.placeOrder, [22, 11, 33]);
  });
});

test("readExperienceManifest keeps rejecting comments as invalid JSON", () => {
  const projectRoot = fs.mkdtempSync(path.join(os.tmpdir(), "renium-experience-json-"));
  fs.writeFileSync(
    path.join(projectRoot, "renium.experience.json"),
    "{\n  // JSONC isn't accepted here\n  \"version\": 2\n}\n",
    "utf8",
  );
  try {
    assert.throws(
      () => readExperienceManifest(projectRoot),
      /Could not read renium\.experience\.json:/,
    );
  } finally {
    fs.rmSync(projectRoot, { recursive: true, force: true });
  }
});
