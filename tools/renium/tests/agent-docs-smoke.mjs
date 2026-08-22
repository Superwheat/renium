import childProcess from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

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

const agents = [rootGuide, ...guideNames.map((name) => fs.readFileSync(path.join(guidesPath, name), "utf8"))].join("\n");
for (const forbidden of ["--help", "rbx a ", "local.renium-", "extensions/local.renium", "extensions\\local.renium"]) {
  if (agents.includes(forbidden)) {
    throw new Error(`Generated agent documentation contains forbidden text: ${forbidden}`);
  }
}

const shortCommands = new Set([
  "ad", "ai", "as", "ba", "bb", "bcl", "bem", "bep", "bg", "bim", "bpack", "br", "bs", "bss",
  "clk", "co", "cs", "dev", "dp", "f", "fmt", "gm", "go", "in", "inp", "ip", "ir", "iu", "js",
  "ky", "l", "lc", "lk", "lka", "lkb", "lkd", "lkp", "lks", "lof", "lon", "lst", "me", "mv", "oc",
  "pa", "pl", "play", "pn", "po", "pr", "ps", "pv", "re", "ro", "rp", "rs", "sc", "sg", "si", "sm",
  "sr", "ss", "status", "sx", "tr", "ty", "ui", "v", "vci", "vcm", "vct", "wait", "wally", "x", "xp",
]);
for (const line of agents.split(/\r?\n/)) {
  const match = line.trim().match(/^rbx\s+(\S+)/);
  if (match && !shortCommands.has(match[1])) {
    throw new Error(`Agent documentation uses a non-short command: ${match[1]}`);
  }
}

const root = fs.mkdtempSync(path.join(os.tmpdir(), "renium-agent-docs-"));
try {
  fs.mkdirSync(path.join(root, "src"));
  fs.writeFileSync(path.join(root, "renium.project.jsonc"), JSON.stringify({ schemaVersion: 1, sourceRoot: "src", tree: {} }));
  childProcess.execFileSync(executable, ["pv"], { cwd: root, stdio: "pipe" });
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}

console.log("Renium agent documentation smoke test passed");
