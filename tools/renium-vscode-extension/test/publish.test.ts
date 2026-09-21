import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("extension.ts", fs.readFileSync(path.resolve("src/extension.ts"), "utf8"), ts.ScriptTarget.Latest, true);
const controller = source.statements.find((node): node is ts.ClassDeclaration => ts.isClassDeclaration(node) && node.name?.text === "RobloxSyncController");
const method = controller?.members.find((node): node is ts.MethodDeclaration => ts.isMethodDeclaration(node) && node.name.getText(source) === "publishPlace");
assert.ok(method);
const implementation = ts.transpileModule(`({${method.getText(source).replace(/^public\s+/, "")}})`, { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;

async function run(cloud: boolean, answer: string | undefined, plan: unknown = { gameId: 123, placeId: 456 }) {
  const commands: string[][] = [];
  const warnings: string[] = [];
  const subject = vm.runInNewContext(implementation, {
    vscode: { window: {
      showQuickPick: async () => ({ cloud }),
      showWarningMessage: async (message: string) => { warnings.push(message); return answer; },
    } },
    recordValue: (value: unknown) => typeof value === "object" && value !== null ? value : undefined,
  });
  subject.getConfig = () => ({ projectRoot: "/project" });
  subject.requireProjectManifest = () => "/project/renium.project.jsonc";
  subject.runProjectCommand = async (_name: string, _command: string, args: string[]) => {
    commands.push(Array.from(args));
    return { result: plan };
  };
  let error: unknown;
  try { await subject.publishPlace(); } catch (caught) { error = caught; }
  return { commands, warnings, error };
}

test("publish cancellation only previews; confirmation pins the reviewed destination", async () => {
  const cancelled = await run(false, undefined);
  assert.equal(cancelled.error, undefined);
  assert.equal(cancelled.commands.length, 1);
  assert.ok(cancelled.commands[0].includes("--dry-run"));
  for (const cloud of [false, true]) {
    const result = await run(cloud, "Publish");
    assert.equal(result.error, undefined);
    assert.equal(result.commands.length, 2);
    assert.ok(result.warnings[0].includes("456") && result.warnings[0].includes("123"));
    const publish = result.commands[1];
    assert.ok(!publish.includes("--dry-run"));
    assert.equal(publish.includes("--open-cloud"), cloud);
    assert.deepEqual(publish.slice(cloud ? -4 : -2), cloud ? ["--universe", "123", "--place-id", "456"] : ["--place", "123:456"]);
  }
});

test("an unconfirmed destination cannot reach the publish prompt or upload", async () => {
  const result = await run(false, "Publish", { ok: false });
  assert.ok(result.error);
  assert.equal(result.commands.length, 1);
  assert.equal(result.warnings.length, 0);
});
