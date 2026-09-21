import assert from "node:assert/strict";
import Module from "node:module";
import { test } from "node:test";

let shared: string | undefined;
let editor: string | undefined;
let fileChanged: (() => void) | undefined;
let configChanged: ((event: unknown) => void) | undefined;
const updates: string[] = [];
const change = (): void => configChanged?.({ affectsConfiguration: () => true });
const loader = Module as unknown as { _load(request: string, parent: NodeModule | null, isMain: boolean): unknown };
const original = loader._load;
loader._load = (request, parent, isMain) => {
  if (request === "vscode") {
    return {
      ConfigurationTarget: { Global: 1 },
      Disposable: class { constructor(public dispose: () => void) {} },
      workspace: {
        onDidChangeConfiguration: (handler: typeof configChanged) => {
          configChanged = handler;
          return { dispose: () => { configChanged = undefined; } };
        },
        getConfiguration: () => ({
          inspect: () => ({ globalValue: editor }),
          update: async (_key: string, value: string, target: number) => {
            assert.equal(target, 1);
            updates.push(value);
            editor = value;
            change();
          },
        }),
      },
    };
  }
  if (request === "fs") {
    return {
      watchFile: (_file: string, _options: unknown, handler: () => void) => { fileChanged = handler; },
      unwatchFile: () => { fileChanged = undefined; },
    };
  }
  if (request === "./sharedConfig") {
    return { userConfigPath: () => "user-config.json", userStudioAudioMode: () => shared };
  }
  return original.call(loader, request, parent, isMain);
};
const { watchStudioAudioSettings } = require("../src/studioAudioSettings") as typeof import("../src/studioAudioSettings");
loader._load = original;
const settle = async (): Promise<void> => {
  for (let index = 0; index < 4; index++) { await new Promise(resolve => setImmediate(resolve)); }
};

test("global audio settings synchronize editor and agent changes without a workspace or feedback writes", async () => {
  shared = editor = undefined;
  updates.length = 0;
  const applied: string[] = [];
  const errors: unknown[] = [];
  const binding = watchStudioAudioSettings(async mode => {
    applied.push(mode);
    shared = mode;
    fileChanged?.();
  }, error => errors.push(error));
  await settle();
  assert.deepEqual(applied, []);
  editor = "auto";
  change();
  await settle();
  assert.deepEqual(applied, ["auto"]);
  shared = "mute";
  fileChanged?.();
  await settle();
  assert.equal(editor, "mute");
  assert.deepEqual(applied, ["auto"]);
  editor = undefined;
  change();
  await settle();
  assert.deepEqual(applied, ["auto", "off"]);
  assert.deepEqual(errors, []);
  binding.dispose();
  assert.equal(fileChanged, undefined);
  assert.equal(configChanged, undefined);
});

test("saved global audio wins over stale editor settings and resumes on activation", async () => {
  shared = "auto";
  editor = "mute";
  const applied: string[] = [];
  const errors: unknown[] = [];
  const binding = watchStudioAudioSettings(async mode => { applied.push(mode); }, error => errors.push(error));
  await settle();
  assert.equal(editor, "auto");
  assert.deepEqual(applied, ["auto"]);
  assert.deepEqual(errors, []);
  binding.dispose();
});
