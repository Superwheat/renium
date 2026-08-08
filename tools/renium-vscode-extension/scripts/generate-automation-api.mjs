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
].join("\n");
fs.writeFileSync(path.join(extensionRoot, "src", "automationProtocol.generated.ts"), generatedTypeScript);

const groups = [
  ["Context", 0, 3],
  ["Sync", 10, 16],
  ["Read", 20, 23],
  ["Edit", 30, 37],
  ["Files", 40, 45],
  ["Studio", 50, 59],
  ["Input", 60, 66],
  ["Project", 70, 74],
  ["Review", 80, 82],
];
const operationLines = groups.flatMap(([label, first, last]) => {
  const operations = registry.operations.filter((operation) => operation.id >= first && operation.id <= last);
  return [`- ${label}: ${operations.map((operation) => `\`${operation.name}\``).join(", ")}`];
});
const agents = `# Renium automation

Use the compact Renium automation API for Studio and project operations. On Windows, the stable launcher is \`%USERPROFILE%\\.renium\\bin\\rbx.cmd\`. On macOS and Linux, it is \`~/.renium/bin/rbx\`. If \`rbx\` is not on \`PATH\`, invoke that launcher directly; do not search extension folders.

## Bind once

Bind the intended project and place before every sequence. The returned \`r.id\` is the daemon-local context ID used as \`CX\` below.

\`\`\`powershell
rbx a bind . 101945566570840
rbx a context CX
\`\`\`

The place selector may be a place ID or \`gameId:placeId\`. If multiple Studio runtimes match, inspect the candidate IDs in \`e.d.candidates\`, put \`root\`, \`place\`, and the exact \`runtime\` in \`bind.json\`, then run \`rbx a bind -J bind.json\`. For an empty folder, set \`bootstrap:true\` in the bind payload; that context permits only \`project-init\` and \`project-validate\` until the project is created. A context becomes stale after daemon restart, project identity changes, runtime disconnect, or a plugin rebuild. Bind again after \`stale_cx\`.

## Operations

${operationLines.join("\n")}

\`pull\` writes Studio into project files. \`push\` writes project files into Studio. Live sync stays two-way. When an operation returns \`rejected\` with \`e.n\` set to \`review-prepare\`, the exact operation needs a receipt before \`review-apply\` can execute it. The \`rbx a\` wrapper handles that receipt for direct commands.

\`\`\`powershell
rbx a pull CX
rbx a push CX
rbx a find CX -J find.json
rbx a tree CX -J tree.json
rbx a inspect CX -J inspect.json
rbx a bb CX Workspace -J ops.json
\`\`\`

## Payloads

Put structured parameters in a JSON file. For example, \`ops.json\` can contain:

\`\`\`json
{
  "ops": [
    { "type": "search", "q": "Door", "limit": 5, "fields": "lookup" },
    { "type": "counts" }
  ]
}
\`\`\`

Use stdin when the payload is generated:

\`\`\`powershell
Get-Content .\\set-property.json | rbx a set-property CX -J -
\`\`\`

\`\`\`sh
rbx a set-property CX -J - < ./set-property.json
\`\`\`

Never put structured JSON directly in a shell argument.

## Compact protocol fields

- Request: \`v\` protocol version, \`id\` request ID, \`op\` numeric opcode, \`cx\` bound context, \`p\` parameters.
- Success: \`ok:1\`, \`ms\` elapsed milliseconds, \`r\` result.
- Failure: \`ok:0\`, \`e.c\` stable code, \`e.m\` message, \`e.rt\` retry flag, \`e.n\` next operation, \`e.d\` details.
- Instance fields: \`id\`, \`n\` name, \`c\` class, \`path\`, \`cc\` child count, \`ch\` child IDs. Request only the fields needed.

Use a stable instance ID after \`find\` or \`tree\`. Duplicate paths require ordinal selectors. Never guess the project, place, runtime, sync direction, or duplicate instance.
`;
fs.writeFileSync(path.join(extensionRoot, "resources", "AGENTS.md"), agents);
fs.writeFileSync(path.join(extensionRoot, "resources", "CLAUDE.md"), "Read and follow AGENTS.md.\n");

const rows = registry.operations.map((operation) => {
  const aliases = (operation.aliases ?? []).join(", ");
  return `| ${operation.id} | \`${operation.name}\` | ${aliases || "-"} | ${operation.review ? "yes" : "no"} |`;
});
const table = [
  "<!-- automation-opcodes:start -->",
  `Protocol version: \`${registry.version}\``,
  "",
  "| ID | Operation | Aliases | Review |",
  "|---:|---|---|:---:|",
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
