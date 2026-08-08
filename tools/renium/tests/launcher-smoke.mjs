import childProcess from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const repository = path.resolve(import.meta.dirname, "..", "..", "..");
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "renium-launcher-smoke-"));
const expected = "a|cap";

const run = (command, args, env) => {
  const output = childProcess.execFileSync(command, args, {
    cwd: repository,
    env: { ...process.env, ...env },
    encoding: "utf8",
  }).trim();
  if (output !== expected) {
    throw new Error(`${command} did not forward automation arguments: ${output}`);
  }
};

const hasCommand = (command) => childProcess.spawnSync(
  process.platform === "win32" ? "where.exe" : "sh",
  process.platform === "win32" ? [command] : ["-c", `command -v ${command}`],
  { stdio: "ignore" },
).status === 0;

try {
  if (process.platform === "win32") {
    const stub = path.join(temporary, "renium-stub.cmd");
    fs.writeFileSync(stub, "@echo off\r\necho %1^|%2\r\n");
    const launcher = path.join(repository, "rbx.cmd");
    run("cmd.exe", ["/d", "/c", "rbx.cmd a cap"], { RENIUM_CLI: stub });
    run("powershell.exe", ["-NoProfile", "-Command", `& '${launcher}' a cap`], { RENIUM_CLI: stub });
    if (hasCommand("pwsh.exe")) {
      run("pwsh.exe", ["-NoProfile", "-Command", `& '${launcher}' a cap`], { RENIUM_CLI: stub });
    }
  } else {
    const stub = path.join(temporary, "renium");
    fs.writeFileSync(stub, "#!/bin/sh\nprintf '%s|%s\\n' \"$1\" \"$2\"\n");
    fs.chmodSync(stub, 0o755);
    const launcher = path.join(repository, "rbx");
    run("bash", [launcher, "a", "cap"], { RENIUM_CLI: stub });
    if (hasCommand("zsh")) {
      run("zsh", [launcher, "a", "cap"], { RENIUM_CLI: stub });
    }
  }
} finally {
  fs.rmSync(temporary, { recursive: true, force: true });
}

console.log("Renium launcher smoke test passed");
