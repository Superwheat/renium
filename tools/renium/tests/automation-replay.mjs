import childProcess from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";

const executable = path.resolve(process.argv[2] ?? path.join("tools", "renium", "target", "debug", process.platform === "win32" ? "renium.exe" : "renium"));
if (!fs.existsSync(executable)) {
  throw new Error(`Renium executable does not exist: ${executable}`);
}

const openPort = async () => await new Promise((resolve, reject) => {
  const server = net.createServer();
  server.once("error", reject);
  server.listen(0, "127.0.0.1", () => {
    const address = server.address();
    server.close((error) => error ? reject(error) : resolve(address.port));
  });
});

const firstPort = await openPort();
let secondPort = await openPort();
while (secondPort === firstPort) {
  secondPort = await openPort();
}
const root = fs.mkdtempSync(path.join(os.tmpdir(), "renium-automation-replay-"));
const project = path.join(root, "renium.project.jsonc");
fs.mkdirSync(path.join(root, "src"));
fs.writeFileSync(project, JSON.stringify({ schemaVersion: 1, sourceRoot: "src", tree: {} }));

const daemon = childProcess.spawn(executable, ["bd", "--editor-stdio", "-w", "0.1", "-P", `${firstPort},${secondPort}`], {
  cwd: root,
  stdio: ["pipe", "pipe", "pipe"],
});
const stdoutLines = [];
const pendingLines = [];
let stderr = "";
daemon.stderr.setEncoding("utf8");
daemon.stderr.on("data", (chunk) => {
  stderr += chunk;
});
readline.createInterface({ input: daemon.stdout }).on("line", (line) => {
  if (!line.trim()) {
    return;
  }
  stdoutLines.push(line);
  pendingLines.shift()?.(line);
});

const nextLine = async () => await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error(`Daemon response timed out. stderr: ${stderr}`)), 3000);
  pendingLines.push((line) => {
    clearTimeout(timer);
    resolve(line);
  });
});
const send = async (request) => {
  daemon.stdin.write(`${JSON.stringify(request)}\n`);
  return JSON.parse(await nextLine());
};
const expect = (condition, message) => {
  if (!condition) {
    throw new Error(message);
  }
};

try {
  const cap = await send({ v: 1, id: 1, op: 0, p: {} });
  expect(cap.ok === 1 && cap.r.ops.length === 54, "cap did not return the checked-in registry");
  const bad = await send({ v: 1, id: 2, op: 999, p: {} });
  expect(bad.ok === 0 && bad.e.c === "bad_op" && bad.e.rt === 0, "bad opcode classification changed");
  const bound = await send({ v: 1, id: 3, op: 1, p: { root } });
  expect(bound.ok === 1 && Number.isInteger(bound.r.id), "bind did not return a context");
  const cx = bound.r.id;
  const context = await send({ v: 1, id: 4, op: 2, cx, p: {} });
  const selectedRoot = fs.realpathSync(context.r.root);
  const expectedRoot = fs.realpathSync(root);
  const selectedStat = fs.statSync(selectedRoot, { bigint: true });
  const expectedStat = fs.statSync(expectedRoot, { bigint: true });
  expect(
    context.ok === 1 && selectedStat.dev === expectedStat.dev && selectedStat.ino === expectedStat.ino,
    `context selected ${selectedRoot} instead of ${expectedRoot}`,
  );
  const studios = await send({ v: 1, id: 5, op: 50, cx, p: {} });
  expect(studios.ok === 1 && Array.isArray(studios.r.studios) && studios.ms < 2000, "studios waited for a bridge");
  const directPush = await send({ v: 1, id: 6, op: 11, cx, p: { destructive: true } });
  expect(directPush.ok === 0 && directPush.e.c === "rejected" && directPush.e.rt === 0, "destructive push skipped review");
  const prepared = await send({ v: 1, id: 7, op: 80, cx, p: { op: 11, p: { destructive: true } } });
  expect(prepared.ok === 1 && prepared.r.name === "push", "review receipt targeted the wrong operation");
  const applied = await send({ v: 1, id: 8, op: 81, cx, p: { reviewId: prepared.r.reviewId } });
  expect(applied.ok === 0 && applied.e.c === "no_studio" && applied.e.rt === 0, "missing Studio was retried or misclassified");
  const newProjectRoot = path.join(root, "new-project");
  fs.mkdirSync(newProjectRoot);
  const bootstrap = await send({ v: 1, id: 9, op: 1, p: { root: newProjectRoot, bootstrap: true } });
  expect(bootstrap.ok === 1 && bootstrap.r.initialized === false, "bind did not create a bootstrap context");
  const bootstrapCx = bootstrap.r.id;
  const bootstrapRead = await send({ v: 1, id: 10, op: 20, cx: bootstrapCx, p: { service: "Workspace" } });
  expect(bootstrapRead.ok === 0 && bootstrapRead.e.c === "no_project", "bootstrap context escaped its operation limit");
  const initialized = await send({ v: 1, id: 11, op: 70, cx: bootstrapCx, p: {} });
  expect(initialized.ok === 1 && fs.existsSync(path.join(newProjectRoot, "renium.project.jsonc")), "project-init failed through a bootstrap context");
  const initializedFiles = fs.readdirSync(newProjectRoot, { recursive: true });
  expect(fs.existsSync(path.join(newProjectRoot, "src")), "project-init did not create the source directory");
  expect(!initializedFiles.some((file) => /\.lua(u)?$/i.test(file)), "project-init generated starter source");
  const staleBootstrap = await send({ v: 1, id: 12, op: 2, cx: bootstrapCx, p: {} });
  expect(staleBootstrap.ok === 0 && staleBootstrap.e.c === "stale_cx", "initialized bootstrap context did not become stale");
  fs.appendFileSync(project, "\n");
  const stale = await send({ v: 1, id: 13, op: 2, cx, p: {} });
  expect(stale.ok === 0 && stale.e.c === "stale_cx", "project changes did not invalidate the context");
  expect([cap, bound, context, studios].every((response) => response.ms < 2000), "safe operations waited for Studio");
  expect(!stderr.includes("--help"), "replay invoked help");
  expect(!stderr.includes("local.renium-"), "replay searched extension folders");
  expect(!stderr.toLowerCase().includes("write buffer"), "replay exposed a write-buffer failure");
  expect(stdoutLines.every((line) => {
    const value = JSON.parse(line);
    return value.v === 1 && (value.ok === 0 || value.ok === 1);
  }), "stdout contained non-protocol output");
} finally {
  daemon.stdin.end();
  await new Promise((resolve) => {
    const timer = setTimeout(() => {
      daemon.kill();
      resolve();
    }, 2000);
    daemon.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
  fs.rmSync(root, { recursive: true, force: true });
}

console.log("Renium automation replay passed");
