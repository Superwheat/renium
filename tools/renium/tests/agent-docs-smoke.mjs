import childProcess from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const repository = path.resolve(import.meta.dirname, "..", "..", "..");
const executable = path.resolve(process.argv[2] ?? path.join(repository, "tools", "renium", "target", "debug", process.platform === "win32" ? "renium.exe" : "renium"));
const agentsPath = path.join(repository, "tools", "renium", "renium-agents.md");
const agents = fs.readFileSync(agentsPath, "utf8");
for (const forbidden of ["--help", "local.renium-", "extensions/local.renium", "extensions\\local.renium"]) {
  if (agents.includes(forbidden)) {
    throw new Error(`Generated AGENTS.md contains forbidden fallback text: ${forbidden}`);
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
for (const [name, payload] of Object.entries({
  "find.json": { service: "Workspace", name: "Door", limit: 5 },
  "tree.json": { service: "Workspace", name: "Name", depth: 2 },
  "inspect.json": { service: "Workspace", settingsId: "editor:missing" },
  "ops.json": { ops: [{ type: "counts" }] },
  "push-selected.json": { changedPaths: ["src/Example.server.luau"] },
  "live.json": { services: "Workspace,ReplicatedStorage,ServerScriptService" },
  "play.json": { players: 2, mode: "play" },
  "server-luau.json": { code: "return game.PlaceId" },
  "client-luau.json": { player: "2", code: "return game.PlaceId" },
  "console.json": { player: "2", limit: 20 },
  "shot.json": { player: "2", output: "shot.png" },
  "record-start.json": { player: "2", output: "test-clip.webp", fps: 12, maxSeconds: 60, quality: 80 },
  "record-end.json": { recordingId: "RECORDING_ID_FROM_RECORD_START" },
  "ui.json": { player: "2", limit: 100 },
  "press.json": { player: "2", path: "PlayerGui.Shop.BuyButton" },
  "type.json": { player: "2", path: "PlayerGui.Chat.Box", text: "hello", enter: true },
  "goto.json": { player: "2", target: "Workspace.Shop.Door" },
  "wait.json": { player: "2", condition: "workspace:GetAttribute('Ready') == true", timeout: 1 },
  "device.json": { action: "status" },
  "input.json": { player: "2", actions: [{ action: "key-press", key: "E" }] },
  "creator-search.json": {
    anonymous: true,
    requests: [{
      method: "GET",
      path: "/toolbox-service/v2/assets:search",
      query: { searchCategoryType: "Model", query: "tree", maxPageSize: 1 },
    }],
  },
  "data-store.json": {
    requests: [{ method: "GET", path: "/cloud/v2/universes/1/data-stores" }],
  },
  "set-property.json": {
    editor: true,
    service: "Workspace",
    className: "Part",
    pathSegments: ["Workspace", "Part"],
    pathOrdinals: [1, 1],
    property: "Name",
    value: "Part",
  },
})) {
  fs.writeFileSync(path.join(root, name), JSON.stringify(payload));
}

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
    .filter((line) => line.startsWith("rbx a ") && !line.includes(" < "));
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
  invoke(["a", "set-property", String(cx), "-J", "-"], fs.readFileSync(path.join(root, "set-property.json")));
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
