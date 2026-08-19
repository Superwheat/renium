import childProcess from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const repository = path.resolve(import.meta.dirname, "..", "..", "..");
const executable = path.resolve(process.argv[2] ?? path.join(repository, "tools", "renium", "target", "debug", process.platform === "win32" ? "renium.exe" : "renium"));
const agentsPath = path.join(repository, "tools", "renium", "renium-agents.md");
const guidesPath = path.join(repository, "tools", "renium", "renium-guides");
const rootGuide = fs.readFileSync(agentsPath, "utf8");
const guideNames = fs.readdirSync(guidesPath)
  .filter((name) => name.endsWith(".md"))
  .sort();
const routedGuideNames = [...rootGuide.matchAll(/`RENIUM\/([^`]+\.md)`/g)]
  .map((match) => match[1])
  .sort();
if (rootGuide.length > 5_000) {
  throw new Error(`Root agent guide is too large: ${rootGuide.length} characters`);
}
if (JSON.stringify(routedGuideNames) !== JSON.stringify(guideNames)) {
  throw new Error(`Root agent guide routes ${routedGuideNames.join(", ")}; expected ${guideNames.join(", ")}`);
}
const agents = [
  rootGuide,
  ...guideNames
    .map((name) => fs.readFileSync(path.join(guidesPath, name), "utf8")),
].join("\n");
for (const forbidden of ["--help", "local.renium-", "extensions/local.renium", "extensions\\local.renium"]) {
  if (agents.includes(forbidden)) {
    throw new Error(`Generated agent documentation contains forbidden fallback text: ${forbidden}`);
  }
}

const openPort = async () => await new Promise((resolve, reject) => {
  const server = net.createServer();
  server.once("error", reject);
  server.listen(0, "127.0.0.1", () => {
    const address = server.address();
    server.close((error) => error ? reject(error) : resolve(address.port));
  });
});
const ports = [];
while (ports.length < 3) {
  const port = await openPort();
  if (!ports.includes(port)) {
    ports.push(port);
  }
}

const root = fs.mkdtempSync(path.join(os.tmpdir(), "renium-agent-docs-"));
fs.mkdirSync(path.join(root, "src"));
fs.writeFileSync(path.join(root, "renium.project.jsonc"), JSON.stringify({ schemaVersion: 1, sourceRoot: "src", tree: {} }));

const environment = {
  ...process.env,
  LOCALAPPDATA: path.join(root, "local"),
  XDG_RUNTIME_DIR: path.join(root, "runtime"),
  XDG_STATE_HOME: path.join(root, "state"),
  RENIUM_DAEMON_HOST: "127.0.0.1",
  RENIUM_DAEMON_CONTROL_PORT: String(ports[0]),
};
fs.mkdirSync(environment.XDG_RUNTIME_DIR, { recursive: true });
fs.mkdirSync(environment.XDG_STATE_HOME, { recursive: true });
const daemon = childProcess.spawn(executable, [
  "bd",
  "--name",
  `agent-docs-${process.pid}`,
  "--control-port",
  String(ports[0]),
  "-P",
  `${ports[1]},${ports[2]}`,
  "-w",
  "0.1",
], { cwd: root, env: environment, stdio: "ignore" });

const waitForControl = async () => {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    const connected = await new Promise((resolve) => {
      const socket = net.createConnection({ host: "127.0.0.1", port: ports[0] });
      socket.once("connect", () => {
        socket.destroy();
        resolve(true);
      });
      socket.once("error", () => resolve(false));
    });
    if (connected) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error("Documentation daemon did not start");
};

const invoke = (args, input) => {
  const result = childProcess.spawnSync(executable, args, {
    cwd: root,
    env: environment,
    input,
    encoding: "utf8",
    timeout: 5000,
  });
  const lines = result.stdout.trim().split(/\r?\n/).filter(Boolean);
  if (lines.length !== 1) {
    throw new Error(`Documentation example emitted ${lines.length} stdout lines: ${result.stdout}`);
  }
  const response = JSON.parse(lines[0]);
  if (response.v !== 1 || (response.ok !== 0 && response.ok !== 1)) {
    throw new Error(`Documentation example did not return protocol JSON: ${lines[0]}`);
  }
  if (response.e && ["bad_req", "bad_op"].includes(response.e.c)) {
    throw new Error(`Documentation example has an invalid shape: ${response.e.m}`);
  }
  return response;
};

try {
  await waitForControl();
  const commands = agents.split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.startsWith("rbx a ") && !/<[A-Z_]+>/.test(line));
  let cx;
  for (const command of commands) {
    const parts = command.split(/\s+/).slice(2).map((part) => part === "CX" ? String(cx) : part);
    const response = invoke(["a", ...parts]);
    if (parts[0] === "bind") {
      cx = response.r.id;
    }
  }
  if (!Number.isInteger(cx)) {
    throw new Error("Generated bind example did not return a context");
  }
  const directRoot = path.join(root, "direct-project");
  fs.mkdirSync(directRoot);
  const directBind = invoke(["a", "bind", directRoot, "700010"]);
  const directAdd = invoke(["a", "place-add", String(directBind.r.id), "700010", "Main Lobby", "--game-id", "800010", "--alias", "main"]);
  if (directAdd.ok !== 1) {
    throw new Error(`Direct place-add failed: ${JSON.stringify(directAdd)}`);
  }
  const directRebind = invoke(["a", "bind", directRoot, "700010"]);
  const directRename = invoke(["a", "place-rename", String(directRebind.r.id), "700010", "lobby"]);
  if (directRename.ok !== 1) {
    throw new Error(`Direct place-rename failed: ${JSON.stringify(directRename)}`);
  }
  const reorderRebind = invoke(["a", "bind", directRoot, "700010"]);
  const directReorder = invoke(["a", "place-reorder", String(reorderRebind.r.id), "700010"]);
  if (directReorder.ok !== 1) {
    throw new Error(`Direct place-reorder failed: ${JSON.stringify(directReorder)}`);
  }
  invoke(["a", "set-property", String(cx), "-J", "-"], JSON.stringify({
    editor: true,
    service: "Workspace",
    className: "Part",
    pathSegments: ["Workspace", "Part"],
    pathOrdinals: [1, 1],
    property: "Name",
    value: "Part",
  }));
} finally {
  daemon.kill();
  await new Promise((resolve) => {
    if (daemon.exitCode !== null) {
      resolve();
      return;
    }
    const timer = setTimeout(resolve, 2000);
    daemon.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
  fs.rmSync(root, { recursive: true, force: true });
}

console.log("Renium agent documentation smoke test passed");
