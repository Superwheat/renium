import childProcess from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";

const repository = path.resolve(import.meta.dirname, "..", "..", "..");
const executable = path.resolve(process.argv[2] ?? path.join(repository, "tools", "renium", "target", "debug", process.platform === "win32" ? "renium.exe" : "renium"));
const agentsPath = path.join(repository, "tools", "renium", "renium-agents.md");
const guidesPath = path.join(repository, "tools", "renium", "renium-guides");
const rootGuide = fs.readFileSync(agentsPath, "utf8");
const guideNames = fs.readdirSync(guidesPath).filter((name) => name.endsWith(".md")).sort();
const routedGuideNames = [...rootGuide.matchAll(/`RENIUM\/([^`]+\.md)`/g)].map((match) => match[1]).sort();

if (rootGuide.length > 5_000) {
  throw new Error(`Root agent guide is too large: ${rootGuide.length} characters`);
}
if (JSON.stringify(routedGuideNames) !== JSON.stringify(guideNames)) {
  throw new Error(`Root agent guide routes ${routedGuideNames.join(", ")}; expected ${guideNames.join(", ")}`);
}

for (const name of guideNames) {
  assert.ok(fs.readFileSync(path.join(guidesPath, name), "utf8").length <= 5_000,
    `${name} exceeds 5,000 characters`);
}

const agents = [rootGuide, ...guideNames.map((name) => fs.readFileSync(path.join(guidesPath, name), "utf8"))].join("\n");
for (const forbidden of ["rbx a ", "local.renium-", "extensions/local.renium", "extensions\\local.renium"]) {
  if (agents.includes(forbidden)) {
    throw new Error(`Generated agent documentation contains forbidden text: ${forbidden}`);
  }
}

// Rust's agent_guide_examples_use_canonical_commands checks inline and fenced
// examples against the actual CLI definitions, without executing commands.

if (process.argv.includes("--offline")) {
  const hash = createHash("sha256");
  for (const file of [agentsPath, ...guideNames.map((name) => path.join(guidesPath, name))]) {
    hash.update(path.relative(path.dirname(agentsPath), file).replaceAll(path.sep, "/"));
    hash.update(Buffer.from([0]));
    hash.update(fs.readFileSync(file));
  }
  const resources = path.join(repository, "tools", "renium-vscode-extension", "resources");
  const expected = `${rootGuide.trimEnd()}\n<!-- renium-instructions: ${hash.digest("hex")} -->\n`;
  assert.ok(expected.length <= 5_000, "Packaged root guide exceeds 5,000 characters");
  assert.equal(fs.readFileSync(path.join(resources, "RENIUM.md"), "utf8"), expected);
  for (const name of guideNames) {
    assert.equal(fs.readFileSync(path.join(resources, "RENIUM", name), "utf8"),
      fs.readFileSync(path.join(guidesPath, name), "utf8"));
  }
  console.log("Renium agent documentation offline checks passed (CLI generation not executed)");
  process.exit(0);
}

const root = fs.mkdtempSync(path.join(os.tmpdir(), "renium-agent-docs-"));
try {
  fs.mkdirSync(path.join(root, "src"));
  fs.writeFileSync(path.join(root, "renium.project.jsonc"), JSON.stringify({ schemaVersion: 1, sourceRoot: "src", tree: {} }));
  childProcess.execFileSync(executable, ["init"], { cwd: root, stdio: "pipe" });
  childProcess.execFileSync(executable, ["pv"], { cwd: root, stdio: "pipe" });
  // Check the instructions agents actually receive, not only their source.
  const generated = fs.readFileSync(path.join(root, "RENIUM.md"), "utf8");
  assert.ok(generated.length <= 5_000, `Generated root guide is too large: ${generated.length} characters`);
  assert.equal(generated.replace(/\n<!-- renium-instructions: [a-f0-9]+ -->\s*$/, "").trimEnd(), rootGuide.trimEnd());
  const packagedRoot = fs.readFileSync(path.join(repository, "tools", "renium-vscode-extension", "resources", "RENIUM.md"), "utf8");
  assert.ok(packagedRoot.length <= 5_000, `Extension root guide is too large: ${packagedRoot.length} characters`);
  assert.equal(packagedRoot, generated, "Extension bundled a stale root guide; run sync-assets.mjs --docs-only");
  for (const name of guideNames) {
    const expected = fs.readFileSync(path.join(guidesPath, name), "utf8");
    assert.equal(fs.readFileSync(path.join(root, "RENIUM", name), "utf8"), expected,
      `CLI generated stale ${name}; refresh its packaged guides`);
    assert.equal(fs.readFileSync(path.join(repository, "tools", "renium-vscode-extension", "resources", "RENIUM", name), "utf8"), expected,
      `Extension bundled stale ${name}; run sync-assets.mjs --docs-only`);
  }
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}

console.log("Renium agent documentation smoke test passed");
