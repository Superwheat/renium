import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { test } from "node:test";
import ts from "typescript";

const source = fs.readFileSync(path.resolve(__dirname, "../../src/extension.ts"), "utf8");
const tree = ts.createSourceFile("extension.ts", source, ts.ScriptTarget.Latest, true);
const names = new Set(["startLiveSync", "startLiveSyncInternal", "stopLiveSync", "stopUnresolvedLiveSync", "setDaemonFileSync"]);
const methods: string[] = [];
function visit(node: ts.Node): void {
  if (ts.isMethodDeclaration(node) && names.has(node.name.getText(tree))) { methods.push(node.getText(tree)); }
  ts.forEachChild(node, visit);
}
visit(tree);
assert.equal(methods.length, names.size);
const compiled = ts.transpileModule(`class Flow { ${methods.join("\n")} }; Flow`, {
  compilerOptions: { target: ts.ScriptTarget.ES2022 },
}).outputText;

function fixture(options: { running?: boolean; unresolved?: boolean; fail?: string; hold?: string } = {}) {
  const calls: string[] = [];
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  let entered!: () => void;
  const waiting = new Promise<void>(resolve => { entered = resolve; });
  const phase = async (name: string) => {
    calls.push(name);
    if (name === options.hold) { entered(); await held; }
    if (name === options.fail) { throw new Error(name); }
  };
  const Flow = vm.runInNewContext(compiled, {
    ensureFileExists() {}, invalidateProjectSourceGraph() {}, loadProjectSourceGraph: () => ({ locations: ["src"] }),
    vscode: { window: { showInformationMessage() {} } },
  });
  const flow = new Flow();
  Object.assign(flow, {
    editorLiveSyncRuntimeEnabled: !!options.running, studioLiveSyncStarted: !!options.running,
    daemonFileSyncEnabled: !!options.running, bridgeServeRequested: !!options.running,
    daemonFileSyncTransition: Promise.resolve(),
    getConfig: () => ({ projectRoot: "fixture", cliPath: "rbx", studioLiveSyncEnabled: true }),
    effectiveLiveSyncConfig: (cfg: unknown) => cfg,
    output: { appendLine() {} }, stopStudioActionPolling() {}, updateStatusBar() {},
    scheduleStudioActionPoll() {}, scheduleDaemonFileSyncStatusPoll() {},
    ensureLiveSyncServeReady: async () => { await phase("serve"); flow.bridgeServeRequested = true; return true; },
    setEditorLiveSyncEnabled: async (enabled: boolean) => { flow.editorLiveSyncRuntimeEnabled = enabled; },
    setDaemonFileSyncNow: async (_cfg: unknown, enabled: boolean, paused: boolean) => {
      await phase(enabled ? paused ? "daemon-paused" : "daemon-on" : "daemon-off");
      flow.daemonFileSyncEnabled = enabled;
      if (!enabled) { flow.liveSyncStartupFilePauseHeld = false; }
      return { daemon: { mode: "live" } };
    },
    resolveReconciliation: async (_cfg: unknown, state: unknown) => { await phase("reconcile"); return options.unresolved ? undefined : state; },
    startStudioLiveSyncRuntime: async () => { await phase("runtime"); flow.studioLiveSyncStarted = true; },
    controlDaemonFileWrites: async (_cfg: unknown, action: string) => { await phase(action); },
    disposeLiveSyncRuntime: async () => { flow.studioLiveSyncStarted = false; },
    stopBridgeDaemon: async () => { calls.push("stop-proxy"); },
  });
  return { flow, calls, waiting, release };
}

test("fresh and already-running Live Sync share startup without repeating pause or Studio start", async () => {
  for (const running of [false, true]) {
    const { flow, calls } = fixture({ running });
    await flow.startLiveSync({ silent: true });
    assert.deepEqual(calls, running ? ["daemon-on", "reconcile"] : ["serve", "daemon-paused", "reconcile", "runtime", "resume"]);
    assert.equal(flow.liveSyncStartupInProgress, false);
    assert.equal(flow.liveSyncStartupFilePauseHeld, false);
  }
});

test("startup conflicts and failures stop sync without leaving file writes paused", async () => {
  for (const options of [{ unresolved: true }, { fail: "runtime" }, { fail: "reconcile" }]) {
    const { flow, calls } = fixture(options);
    if (options.fail) { await assert.rejects(flow.startLiveSync({ silent: true }), new RegExp(options.fail)); }
    else { await flow.startLiveSync({ silent: true }); }
    assert.ok(calls.includes("daemon-off"));
    assert.equal(flow.editorLiveSyncRuntimeEnabled, false);
    assert.equal(flow.daemonFileSyncEnabled, false);
    assert.equal(flow.liveSyncStartupFilePauseHeld, false);
    assert.equal(flow.liveSyncStartPromise, undefined);
  }
});

test("rapid duplicate starts coalesce and stopping during reconciliation cannot resume sync", async () => {
  const { flow, calls, waiting, release } = fixture({ hold: "reconcile" });
  const start = flow.startLiveSync({ silent: true });
  const duplicate = flow.startLiveSync({ silent: true });
  await waiting;
  const stop = flow.stopLiveSync({ silent: true });
  release();
  await Promise.all([start, duplicate, stop]);
  assert.equal(calls.filter(call => call === "serve").length, 1);
  assert.ok(!calls.includes("runtime") && !calls.includes("resume"));
  assert.equal(flow.editorLiveSyncRuntimeEnabled, false);
  assert.equal(flow.daemonFileSyncEnabled, false);
  assert.equal(flow.liveSyncStartupFilePauseHeld, false);
});
