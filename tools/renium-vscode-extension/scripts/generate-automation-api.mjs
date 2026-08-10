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

## Project files

A single-place project keeps editable scripts under \`src\`. A multi-place project has \`renium.experience.json\` and one source tree under \`places/<alias>/src\` per place. Bind the experience root and place ID; the returned context resolves the right source tree, so later operations must not infer it from the shell directory.

Edit normal script files directly. Do not read, decode, edit, move, or replace files in \`.renium\`, and do not edit \`sourcemap.json\`; both are generated. Query or mutate their stored DataModel through the operations below.

## Operations

${operationLines.join("\n")}

\`pull\` writes Studio into project files. \`push\` writes project files into Studio. Live sync stays two-way. When an operation returns \`rejected\` with \`e.n\` set to \`review-prepare\`, the exact operation needs a receipt before \`review-apply\` can execute it. The \`rbx a\` wrapper handles that receipt for direct commands.

## Sync and read

\`\`\`powershell
rbx a pull CX
rbx a push CX
rbx a find CX -J find.json
rbx a tree CX -J tree.json
rbx a inspect CX -J inspect.json
rbx a bb CX Workspace -J ops.json
\`\`\`

Start with a bounded high-level query. Example \`find.json\`:

\`\`\`json
{ "service": "Workspace", "className": "Script", "limit": 5, "fields": "lookup" }
\`\`\`

Example \`tree.json\`:

\`\`\`json
{ "service": "Workspace", "name": "StoredData", "depth": 2, "limit": 100 }
\`\`\`

Example \`inspect.json\` after a query returned a stable ID:

\`\`\`json
{ "service": "Workspace", "settingsId": "editor:id", "fields": "brief,prop:Name,attr:Tags" }
\`\`\`

Prefer \`service\` for project reads and edits. Use \`file\` only when a specific settings file is required, and never provide both in one payload.

Use \`batch\` or \`bb\` for several low-level reads in one request. Example \`ops.json\`:

\`\`\`json
{
  "ops": [
    { "type": "search", "q": "Door", "limit": 5, "fields": "lookup" },
    { "type": "counts" }
  ]
}
\`\`\`

Batch op types are \`counts\`, \`service\`, \`search\`, \`find\`, \`children\`, and \`instance\`. Useful field groups:

\`\`\`text
lookup = id,n,c,path
tree   = id,n,c,cc,ch
brief  = id,n,c,path,cc
prop:X = one property
attr:X = one attribute
\`\`\`

Batch requests also accept the compact aliases \`op\` or \`kind\` for \`type\`, \`rid\` for \`requestId\`, \`q\` for \`query\`, \`l\` for \`limit\`, \`id\` for \`settingsId\`, \`x\` for \`index\`, \`n\` for \`name\`, \`c\` for \`className\`, \`pid\` for \`parentSettingsId\`, \`path\` for \`pathSegments\`, \`ords\` for \`pathOrdinals\`, \`props\` for \`properties\`, and \`attrs\` for \`attributes\`.

For a duplicate path, include ordinals in a batch op:

\`\`\`json
{ "ops": [{ "type": "instance", "path": ["Workspace", "Door"], "ords": [2], "fields": "brief" }] }
\`\`\`

Request only the values needed. \`find\` also accepts \`name\`, \`className\`, \`parentSettingsId\`, \`tag\`, repeated \`property\` filters, and repeated \`attribute\` filters. A filter is either a name or \`NAME=JSON\`.

## Selectors and edits

Read and bytecode edit operations select one instance with \`settingsId\`, \`index\`, \`name\`, \`className\`, or \`pathSegments\` plus optional \`pathOrdinals\`. Do not combine a path with another selector. Project-aware clone, move, and remove operations require \`settingsId\`. Duplicate names and paths must use a stable ID or ordinals; find the ID again after reconnecting or doing a full sync.

The edit operations accept these main payload fields:

- \`get-property\`: \`service\`, one selector, \`property\`, optional \`scope\`.
- \`set-property\`: the same fields plus \`value\`.
- \`set-source\`: \`service\`, one selector, and either \`source\` or \`sourceFile\`.
- \`add\`: \`service\`, \`name\`, \`className\`, and optional \`parentSettingsId\`.
- \`clone\`: \`service\`, \`settingsId\`, and \`parentSettingsId\`.
- \`move\`: \`service\`, \`settingsId\`, \`parentSettingsId\`, and optional \`targetService\`.
- \`remove\`: \`service\` and \`settingsId\`.

Example \`set-property.json\`:

\`\`\`json
{ "service": "Workspace", "settingsId": "editor:id", "property": "DisplayName", "value": "VIP Man" }
\`\`\`

Store edits change project files. Push only selected edited files immediately with \`push-selected.json\`:

\`\`\`json
{ "changedPaths": ["src/ServerScriptService/Example.server.luau"] }
\`\`\`

\`\`\`powershell
rbx a set-property CX -J set-property.json
rbx a push CX -J push-selected.json
\`\`\`

Use \`editor:true\` only with \`set-property\` or \`remove\` when the operation must target the bound live Studio instance directly. Verify every mutation with \`inspect\`, \`get-property\`, or another bounded read.

\`revert\` restores project data by \`path\` or by \`service\` plus \`settingsId\`; run \`push\` separately when the restored files should be sent to Studio. Model operations use these fields:

- \`import-model\`: \`service\`, \`parentSettingsId\`, \`model\`, optional \`overridePackages\`.
- \`export-model\`: \`service\`, \`settingsId\`, \`output\`, optional \`format\` (\`rbxm\` or \`rbxmx\`).
- \`export-place\`: \`output\`, optional \`services\` and \`format\` (\`rbxl\` or \`rbxlx\`).
- \`import-snapshots\`: optional \`snapshotDir\`, \`services\`, \`threads\`, and \`noProjectWrite\`.
- \`export-snapshots\`: the same export controls as \`pull\`, but it does not import the snapshots into source files.
- \`sourcemap\`: optional \`output\`, \`stdout\`, \`absolutePaths\`, and \`filters\`.

## Live sync

Live sync is always two-way. Choose its initial direction explicitly: run \`pull\` first for Studio priority, \`push\` first for editor priority, or neither for no initial sync. Then start the watcher:

\`\`\`powershell
rbx a live-start CX -J live.json
rbx a live-status CX
rbx a retry-pending CX
rbx a discard-pending CX
rbx a live-stop CX
\`\`\`

Example \`live.json\`:

\`\`\`json
{ "services": "Workspace,ReplicatedStorage,ServerScriptService" }
\`\`\`

## Play, console, and screenshots

\`studios\` returns matching edit runtimes and the bound runtime's play server and client entries. Example \`play.json\`:

\`\`\`json
{ "players": 2, "mode": "play" }
\`\`\`

Example \`server-luau.json\`:

\`\`\`json
{ "code": "print(game.JobId)" }
\`\`\`

Example \`client-luau.json\`:

\`\`\`json
{ "player": "2", "code": "print(game.Players.LocalPlayer.Name)" }
\`\`\`

\`\`\`powershell
rbx a studios CX
rbx a play-start CX -J play.json
rbx a luau CX -J server-luau.json
rbx a luau CX -J client-luau.json
rbx a console CX -J console.json
rbx a shot CX -J shot.json
rbx a play-stop CX
\`\`\`

Without \`player\`, \`luau\` runs on the edit runtime or play server. \`player\` accepts a player name or one-based client index. Luau returns values and captured output; compile errors, runtime errors, and timeouts fail the operation. Keep snippets action-first and direct, avoid setup and helper wrappers unless needed, and poll only values that can be absent. Prefer \`wait()\` over \`task.wait()\` when they are equivalent. \`console.json\` may use \`player\`, \`limit\`, \`sinceSeq\`, \`grep\`, and \`level\`. \`shot.json\` may use \`player\` and \`output\`, or \`studio:true\` for the edit viewport. Screenshots do not require Studio or the play client to have focus. Each Luau execution stops retained threads started by the previous execution in the same context.

## UI, world, and device input

Inspect visible UI before interacting. Prefer a returned path or ID over coordinates. Relevant payloads are:

- \`ui\`: optional \`player\`, \`limit\`, and \`includeOffscreen\`.
- \`press\`: \`path\` or \`id\`, optional \`player\`, \`world\`, \`right\`, and \`hold\`.
- \`click\`: \`x\`, \`y\`, and optional \`player\`, \`right\`, and \`hold\`.
- \`key\`: \`key\` and optional \`player\` and \`holdMs\`.
- \`type\`: \`text\`, optional TextBox \`path\`, \`player\`, and \`enter\`.
- \`goto\`: \`target\` or \`pos\`, optional \`player\`, \`tp\`, and \`timeout\`.
- \`wait\`: \`condition\`, optional \`player\` or \`client\`, \`timeout\`, and \`interval\`.
- \`device\`: \`action\` is \`list\`, \`status\`, \`set\`, or \`stop\`; setting accepts \`device\`, \`orientation\`, \`scalingMode\`, \`resolution\`, and \`pixelDensity\`.

\`\`\`powershell
rbx a ui CX -J ui.json
rbx a press CX -J press.json
rbx a type CX -J type.json
rbx a goto CX -J goto.json
rbx a wait CX -J wait.json
rbx a device CX -J device.json
\`\`\`

Example payloads:

\`\`\`json
{ "player": "2", "limit": 100 }
{ "player": "2", "path": "PlayerGui.Shop.BuyButton" }
{ "player": "2", "path": "PlayerGui.Chat.Box", "text": "hello", "enter": true }
\`\`\`

\`ui\` returns on-screen controls unless \`includeOffscreen:true\` is set. A duplicate path may use \`Name[n]\`; an ambiguous path fails with candidates, while one visible match is selected. \`press\` scrolls a control into view. For a world target, use \`goto\` before \`press\` with \`world:true\`; world presses fail when the target remains off-screen. Injected clicks cannot trigger \`ClickDetector\` because Roblox does not expose that input path. Keys accept letters, digits, \`Space\`, \`Enter\`, \`Escape\`, \`Tab\`, arrows, \`Shift\`, \`Ctrl\`, and \`Alt\`.

Device emulation uses Studio's simulator without focus or ribbon interaction. While it is active, \`shot\` defaults to the simulated viewport. Named and stable device IDs are accepted, including notched devices for safe-area checks.

Example \`goto.json\`, \`wait.json\`, and \`device.json\` payloads:

\`\`\`json
{ "player": "2", "target": "Workspace.Shop.Door" }
{ "player": "2", "condition": "workspace:GetAttribute('Ready') == true", "timeout": 10 }
{ "action": "set", "device": "iPhone 16 Pro", "orientation": "portrait", "scalingMode": "fit" }
\`\`\`

## Project and place management

Use a bootstrap context for \`project-init\`, then bind the created project again. \`project-validate\` checks the bound project without starting Studio. \`place-add\` takes \`placeId\`, \`name\`, and optional \`gameId\`, \`alias\`, and \`root\`. \`place-rename\` takes \`placeId\` and \`alias\`. \`place-reorder\` takes \`order\`, an array containing every published place ID exactly once.

\`studio-open\`, \`studio-close\`, and destructive replacement require a review receipt. They never run as an automatic recovery route after another operation fails.

## Payload files

Put structured parameters in a JSON file. Use stdin when the payload is generated:

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
- Batch top-level fields: \`f\` settings file, \`s\` service, \`rs\` results, \`t\` op type, \`q\` request ID.
- Instance fields: \`id\`, \`x\` index, \`n\` name, \`c\` class, \`pid\`/\`px\` parent, \`path\`, \`ords\`, \`cc\` child count, \`ch\` child IDs, \`src\`, \`props\`, and \`attrs\`. Request only the fields needed.

Use a stable instance ID after \`find\` or \`tree\`. Duplicate paths require ordinal selectors. Never guess the project, place, runtime, sync direction, or duplicate instance. Treat \`ok:0\` or a nonzero wrapper exit as a failed operation; use the returned error and never retry by switching to a different operation.
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
