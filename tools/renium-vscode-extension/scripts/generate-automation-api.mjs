import fs from "node:fs";
import path from "node:path";

const extensionRoot = path.resolve(import.meta.dirname, "..");
const repositoryRoot = path.resolve(extensionRoot, "..", "..");
const registryPath = path.join(repositoryRoot, "tools", "renium", "protocol", "opcodes.json");
const registry = JSON.parse(fs.readFileSync(registryPath, "utf8"));

if (registry.version !== 1 || !Array.isArray(registry.operations)) {
  throw new Error("Invalid Renium automation opcode registry");
}

const ids = new Set();
const names = new Set();
for (const operation of registry.operations) {
  if (!Number.isInteger(operation.id) || ids.has(operation.id)) {
    throw new Error(`Duplicate or invalid opcode ${operation.id}`);
  }
  ids.add(operation.id);
  for (const name of [operation.name, ...(operation.aliases ?? [])]) {
    if (names.has(name)) {
      throw new Error(`Duplicate operation name ${name}`);
    }
    names.add(name);
  }
}

const propertyName = (name) => name.replace(/-([a-z])/g, (_, character) => character.toUpperCase());
const generatedTypeScript = [
  `export const AUTOMATION_PROTOCOL_VERSION = ${registry.version} as const;`,
  "",
  "export const AUTOMATION_OP = {",
  ...registry.operations.map((operation) => `  ${propertyName(operation.name)}: ${operation.id},`),
  "} as const;",
  "",
  "export const AUTOMATION_RUNTIME_OPS = new Set<number>([",
  ...registry.operations.filter((operation) => operation.runtime).map((operation) => `  ${operation.id},`),
  "]);",
  "",
].join("\n");
fs.writeFileSync(path.join(extensionRoot, "src", "automationProtocol.generated.ts"), generatedTypeScript);

const groups = [
  ["Context", 0, 3],
  ["Sync", 10, 16],
  ["Read", 20, 26],
  ["Edit", 30, 38],
  ["Files", 40, 45],
  ["Studio", 50, 59],
  ["Input and capture", 60, 69],
  ["Project", 70, 74],
  ["Review", 80, 82],
  ["Roblox Cloud and creator assets", 90, 97],
];
const operationLines = groups.flatMap(([label, first, last]) => {
  const operations = registry.operations.filter((operation) => operation.id >= first && operation.id <= last);
  return [`- ${label}: ${operations.map((operation) => `\`${operation.name}\``).join(", ")}`];
});
const agentsPath = path.join(repositoryRoot, "tools", "renium", "renium-agents.md");
const operationsStart = "<!-- automation-operations:start -->";
const operationsEnd = "<!-- automation-operations:end -->";
let agents = fs.readFileSync(agentsPath, "utf8");
const operationsStartIndex = agents.indexOf(operationsStart);
const operationsEndIndex = agents.indexOf(operationsEnd);
if (operationsStartIndex < 0 || operationsEndIndex < operationsStartIndex) {
  throw new Error("Renium agent instructions are missing operation markers");
}
const generatedOperations = `${operationsStart}\n${operationLines.join("\n")}\n${operationsEnd}`;
agents = agents.slice(0, operationsStartIndex) + generatedOperations + agents.slice(operationsEndIndex + operationsEnd.length);
fs.writeFileSync(agentsPath, agents);

const rows = registry.operations.map((operation) => {
  const aliases = (operation.aliases ?? []).join(", ");
  return `| ${operation.id} | \`${operation.name}\` | ${aliases || "-"} | ${operation.review ? "yes" : "no"} | ${operation.runtime ? "yes" : "no"} |`;
});
const table = [
  "<!-- automation-opcodes:start -->",
  `Protocol version: \`${registry.version}\``,
  "",
  "| ID | Operation | Aliases | Review | Studio |",
  "|---:|---|---|:---:|:---:|",
  ...rows,
  "<!-- automation-opcodes:end -->",
].join("\n");
const readmePath = path.join(repositoryRoot, "tools", "renium", "README.md");
let readme = fs.readFileSync(readmePath, "utf8");
const pattern = /<!-- automation-opcodes:start -->[\s\S]*?<!-- automation-opcodes:end -->/;
readme = pattern.test(readme)
  ? readme.replace(pattern, table)
  : `${readme.trimEnd()}\n\n## Automation opcode registry\n\n${table}\n`;
fs.writeFileSync(readmePath, readme);
