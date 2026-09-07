import childProcess from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const repository = path.resolve(import.meta.dirname, "..", "..", "..");
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "renium-launcher-smoke-"));
const expected = "f|Workspace";

const run = (command, args, env) => {
  const output = childProcess.execFileSync(command, args, {
    cwd: repository,
    env: { ...process.env, ...env },
    encoding: "utf8",
  }).trim();
  if (output !== expected) {
    throw new Error(`${command} did not forward CLI arguments: ${output}`);
  }
};

const hasCommand = (command) => childProcess.spawnSync(
  process.platform === "win32" ? "where.exe" : "sh",
  process.platform === "win32" ? [command] : ["-c", `command -v ${command}`],
  { stdio: "ignore" },
).status === 0;

try {
  if (process.platform === "win32") {
    fs.writeFileSync(path.join(temporary, "fixture-asset"), "Renium installer fixture\n");
    childProcess.execFileSync(hasCommand("pwsh.exe") ? "pwsh.exe" : "powershell.exe", [
      "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", path.join(repository, "tools", "renium", "tests", "installer-manifest.ps1"),
      "-Installer", path.join(repository, "install.ps1"), "-Fixture", temporary,
    ], { stdio: "inherit" });
    const stub = path.join(temporary, "renium-stub.cmd");
    fs.writeFileSync(stub, "@echo off\r\necho %1^|%2\r\n");
    const launcher = path.join(repository, "rbx.cmd");
    run("cmd.exe", ["/d", "/c", "rbx.cmd f Workspace"], { RENIUM_CLI: stub });
    run("powershell.exe", ["-NoProfile", "-Command", `& '${launcher}' f Workspace`], { RENIUM_CLI: stub });
    if (hasCommand("pwsh.exe")) {
      run("pwsh.exe", ["-NoProfile", "-Command", `& '${launcher}' f Workspace`], { RENIUM_CLI: stub });
    }
  } else {
    const stub = path.join(temporary, "renium");
    fs.writeFileSync(stub, "#!/bin/sh\nprintf '%s|%s\\n' \"$1\" \"$2\"\n");
    fs.chmodSync(stub, 0o755);
    const launcher = path.join(repository, "rbx");
    run("bash", [launcher, "f", "Workspace"], { RENIUM_CLI: stub });
    if (hasCommand("zsh")) {
      run("zsh", [launcher, "f", "Workspace"], { RENIUM_CLI: stub });
    }

    const installed = path.join(temporary, "installed");
    const stable = path.join(temporary, "stable");
    fs.mkdirSync(installed);
    fs.mkdirSync(stable);
    fs.copyFileSync(launcher, path.join(installed, "rbx"));
    fs.copyFileSync(stub, path.join(installed, "renium"));
    fs.chmodSync(path.join(installed, "rbx"), 0o755);
    fs.chmodSync(path.join(installed, "renium"), 0o755);
    fs.symlinkSync(path.join(installed, "rbx"), path.join(stable, "rbx"));
    fs.mkdirSync(path.join(stable, "renium"));
    run(path.join(stable, "rbx"), ["f", "Workspace"], {
      RENIUM_CLI: "",
      XDG_DATA_HOME: path.join(temporary, "missing-data-home"),
    });
  }
} finally {
  fs.rmSync(temporary, { recursive: true, force: true });
}

console.log("Renium launcher smoke test passed");
