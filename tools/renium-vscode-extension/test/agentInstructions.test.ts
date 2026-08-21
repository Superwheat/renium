import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";

import { ensureReniumAgentInstructions } from "../src/agentInstructions";

test("Renium guide uses one marked pointer in project instructions", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "renium-agent-instructions-"));
  const extension = path.join(root, "extension");
  const project = path.join(root, "project");
  const pointer = "\u2063Read and follow RENIUM.md.\u2063\n";
  fs.mkdirSync(path.join(extension, "resources"), { recursive: true });
  fs.mkdirSync(path.join(extension, "resources", "RENIUM"));
  fs.mkdirSync(project);
  fs.writeFileSync(path.join(project, "renium.project.jsonc"), "{\"schemaVersion\":1}\n");
  fs.writeFileSync(path.join(extension, "resources", "RENIUM.pointer.md"), pointer);
  fs.writeFileSync(
    path.join(extension, "resources", "RENIUM.md"),
    "<!-- renium-version: 0.2.6 -->\n# Current guide\n",
  );
  fs.writeFileSync(path.join(extension, "resources", "RENIUM", "data.md"), "# Data guide\n");

  const agents = path.join(project, "AGENTS.md");
  const claude = path.join(project, "CLAUDE.md");
  fs.writeFileSync(agents, "# Old\n\nrenium-0.1.4\n");
  fs.writeFileSync(claude, "Read and follow AgEnTs.Md.\n");
  assert.deepEqual(ensureReniumAgentInstructions(extension, project), [
    path.join(project, "RENIUM.md"),
    path.join(project, "RENIUM", "data.md"),
    agents,
  ]);
  assert.equal(
    fs.readFileSync(path.join(project, "RENIUM.md"), "utf8"),
    "<!-- renium-version: 0.2.6 -->\n# Current guide\n",
  );
  assert.equal(fs.readFileSync(path.join(project, "RENIUM", "data.md"), "utf8"), "# Data guide\n");
  assert.equal(fs.readFileSync(agents, "utf8"), pointer);
  assert.equal(fs.readFileSync(claude, "utf8"), "Read and follow AgEnTs.Md.\n");

  fs.writeFileSync(agents, "# Project rules\n");
  fs.writeFileSync(claude, "# Claude rules\n");
  assert.deepEqual(ensureReniumAgentInstructions(extension, project), [agents, claude]);
  assert.equal(fs.readFileSync(agents, "utf8"), `# Project rules\n\n${pointer}`);
  assert.equal(fs.readFileSync(claude, "utf8"), `# Claude rules\n\n${pointer}`);
  assert.deepEqual(ensureReniumAgentInstructions(extension, project), []);

  const ordinary = path.join(root, "ordinary");
  fs.mkdirSync(ordinary);
  assert.deepEqual(ensureReniumAgentInstructions(extension, ordinary), []);
  assert.equal(fs.existsSync(path.join(ordinary, "AGENTS.md")), false);
  fs.rmSync(root, { recursive: true, force: true });
});
