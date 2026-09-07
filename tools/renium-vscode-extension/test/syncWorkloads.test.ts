import assert from "node:assert/strict";
import Module from "node:module";
import { test } from "node:test";
import * as path from "node:path";

const loader = Module as unknown as { _load(request: string, parent: NodeModule | null, isMain: boolean): unknown };
const original = loader._load;
let queryCount = 0;
let query: Record<string, unknown> = {};
let queryResult = Promise.resolve({ sourcePaths: ["a.luau", "b.luau"] });
loader._load = (request, parent, isMain) => {
  if (request === "vscode") { return {}; }
  if (request === "./fileExplorer") { return { logPackageDragDebug: () => undefined }; }
  if (request === "./fileExplorerCore") {
    return {
      getExplorerConfig: () => ({ projectRoot: "test" }),
      settingsFileForService: () => "test.renium",
      runBytecodeBatchOne: (_cfg: unknown, _file: unknown, _service: unknown, op: Record<string, unknown>) => {
        queryCount += 1;
        query = op;
        return queryResult;
      },
    };
  }
  return original.call(loader, request, parent, isMain);
};
const { PackageSyncController } = require("../src/packageSyncController") as typeof import("../src/packageSyncController");
const { FileExplorerModel } = require("../src/fileExplorerModel") as typeof import("../src/fileExplorerModel");
loader._load = original;

type Pending = { projectRoot: string; generation: number };
type TestController = {
  pendingLinkPackageSourcePaths: Map<string, Pending>;
  flushLinkPackageSourceChanges(root: string, generation: number): Promise<void>;
  linkApply(): Promise<void>;
};
const root = path.resolve("test-project");
const source = path.join(root, "shared.renium");
function controller(): TestController {
  return Object.assign(Object.create(PackageSyncController.prototype), {
    host: {
      getConfig: () => ({ projectRoot: root }),
      experienceGeneration: () => 1,
      experienceChanging: () => false,
      output: { appendLine: () => undefined },
    },
    pendingLinkPackageSourcePaths: new Map([[source, { projectRoot: root, generation: 1 }]]),
    invalidateLinkStatusCache: () => undefined,
    resolveLinkStatus: async () => ({ kind: "success", value: { links: [{ id: "shared", sourcePath: source, activeTargetCount: 1 }] } }),
    absoluteLinkSourcePath: (_cfg: unknown, file: string) => file,
    refreshFileExplorerSafe: async () => undefined,
    scheduleLinkPackageSourceFlush: () => undefined,
    linkApply: async () => undefined,
  }) as TestController;
}

test("new save during an apply remains pending for the next flush", async () => {
  const c = controller();
  let applied = 0;
  c.linkApply = async () => {
    applied += 1;
    if (applied === 1) { c.pendingLinkPackageSourcePaths.set(source, { projectRoot: root, generation: 1 }); }
  };
  await c.flushLinkPackageSourceChanges(root, 1);
  assert.equal(c.pendingLinkPackageSourcePaths.size, 1);
  await c.flushLinkPackageSourceChanges(root, 1);
  assert.equal(applied, 2);
  assert.equal(c.pendingLinkPackageSourcePaths.size, 0);
});

test("overlapping flushes serialize and do not repeat acknowledged events", async () => {
  const c = controller();
  let active = 0;
  let peak = 0;
  let applies = 0;
  c.linkApply = async () => {
    active += 1;
    applies += 1;
    peak = Math.max(peak, active);
    await new Promise<void>((resolve) => setImmediate(resolve));
    active -= 1;
  };
  await Promise.all(Array.from({ length: 100 }, () => c.flushLinkPackageSourceChanges(root, 1)));
  assert.equal(peak, 1);
  assert.equal(applies, 1);
});

test("failed apply preserves pending work and rejects stale project generations", async () => {
  const c = controller();
  c.linkApply = async () => { throw new Error("backend unavailable"); };
  await c.flushLinkPackageSourceChanges(root, 1);
  assert.equal(c.pendingLinkPackageSourcePaths.size, 1);
  let applied = false;
  c.linkApply = async () => { applied = true; };
  await c.flushLinkPackageSourceChanges(root, 0);
  assert.equal(applied, false);
  await c.flushLinkPackageSourceChanges(root, 1);
  assert.equal(applied, true);
});

test("subtree sources use one query regardless of loaded child count", async () => {
  const model = new FileExplorerModel();
  queryCount = 0;
  const node = { kind: "instance", service: "Workspace", settingsId: "canonical", projectionSettingsId: "projected", children: Array(1000).fill("child") };
  const sources = await model.sourcePathsForSubtree(node as Parameters<typeof model.sourcePathsForSubtree>[0]);
  assert.deepEqual(sources, ["a.luau", "b.luau"]);
  assert.equal(queryCount, 1);
  assert.deepEqual(query, { type: "sources", id: "projected" });
});

test("subtree response from a replaced project is ignored", async () => {
  const model = new FileExplorerModel();
  let complete!: (value: { sourcePaths: string[] }) => void;
  queryResult = new Promise((resolve) => { complete = resolve; });
  const request = model.sourcePathsForSubtree({ kind: "service", service: "Workspace" } as Parameters<typeof model.sourcePathsForSubtree>[0]);
  model.resetProjectState();
  complete({ sourcePaths: ["old-project.luau"] });
  assert.deepEqual(await request, []);
});
