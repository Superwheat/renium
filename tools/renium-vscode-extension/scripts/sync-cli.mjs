import childProcess from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const extensionRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(extensionRoot, "..", "..");
const binaryName = process.platform === "win32" ? "renium.exe" : "renium";
const configuredSource = process.env.RENIUM_CLI_BUILD?.trim();
const candidates = [
  configuredSource,
  path.join(repoRoot, "tools", "renium", "target", "release", binaryName),
  path.join(repoRoot, binaryName),
].filter(Boolean);
const source = candidates.find((candidate) => fs.existsSync(candidate) && fs.statSync(candidate).isFile());
if (!source) {
  throw new Error(`Renium CLI was not found. Build tools/renium in release mode or set RENIUM_CLI_BUILD.`);
}

const packageJson = JSON.parse(fs.readFileSync(path.join(extensionRoot, "package.json"), "utf8"));
const versionResult = childProcess.spawnSync(source, ["--version"], {
  encoding: "utf8",
  windowsHide: true,
});
const versionOutput = `${versionResult.stdout ?? ""}\n${versionResult.stderr ?? ""}`;
const versionMatch = versionOutput.match(/^renium\s+(\d+\.\d+\.\d+)\s*$/m);
if (versionResult.status !== 0 || versionMatch?.[1] !== String(packageJson.version)) {
  throw new Error(`Renium CLI at ${source} does not match extension v${packageJson.version}.`);
}

const targetPlatform = process.env.RENIUM_CLI_TARGET_PLATFORM?.trim() || process.platform;
const targetArchitecture = process.env.RENIUM_CLI_TARGET_ARCH?.trim() || process.arch;
const destinationDir = path.join(extensionRoot, "bin", `${targetPlatform}-${targetArchitecture}`);
fs.mkdirSync(destinationDir, { recursive: true });
const destination = path.join(destinationDir, binaryName);
fs.copyFileSync(source, destination);
if (process.platform === "win32") {
  fs.copyFileSync(path.join(repoRoot, "rbx.cmd"), path.join(destinationDir, "rbx.cmd"));
  fs.copyFileSync(
    path.join(repoRoot, "tools", "renium", "rbx-run.ps1"),
    path.join(destinationDir, "rbx-run.ps1"),
  );
} else {
  const launcher = path.join(destinationDir, "rbx");
  fs.copyFileSync(path.join(repoRoot, "rbx"), launcher);
  fs.chmodSync(destination, 0o755);
  fs.chmodSync(launcher, 0o755);
}

process.stdout.write(`Bundled ${source} as ${destination}\n`);
