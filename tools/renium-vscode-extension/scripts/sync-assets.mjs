import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

import { generateRobloxPropertiesMetadata } from "./generate-properties-metadata.mjs";

const extensionRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(extensionRoot, "..", "..");
const extensionAssets = path.join(extensionRoot, "assets");
const extensionResources = path.join(extensionRoot, "resources");
const logoFiles = [
  "logo-white.png",
  "logo-black.png",
  "renium-black-superbold.png",
  "renium-white-superbold.png",
];
const insertableObjectsIconTheme = process.env.RENIUM_INSERTABLE_OBJECTS_ICON_THEME ?? "Dark";
const insertableObjectsIconSize = process.env.RENIUM_INSERTABLE_OBJECTS_ICON_SIZE ?? "Standard";
const preferredIconScale = process.env.RENIUM_INSERTABLE_OBJECTS_ICON_SCALE ?? "@2x";
const refreshStudioApi =
  process.argv.includes("--refresh-studio-api") ||
  process.env.RENIUM_REFRESH_STUDIO_API === "1";

function syncStudioPluginBundle() {
  const configured = process.env.RENIUM_PLUGIN_BUNDLE?.trim();
  const generated = !configured;
  const source = configured || path.join(os.tmpdir(), `renium-plugin-${process.pid}-${Date.now()}.rbxm`);
  if (generated) {
    const project = path.join(repoRoot, "tools", "plugin_ws_bridge", "Renium.project.json");
    const result = spawnSync("rojo", ["build", project, "--output", source], { stdio: "inherit" });
    if (result.error) {
      throw result.error;
    }
    if (result.status !== 0) {
      throw new Error(`Rojo exited with code ${result.status}`);
    }
  }
  try {
    const bytes = fs.readFileSync(source);
    if (bytes.length < 16 || bytes.subarray(0, 7).toString("ascii") !== "<roblox") {
      throw new Error(`Invalid Studio plugin bundle: ${source}`);
    }
    fs.copyFileSync(source, path.join(extensionAssets, "Renium.rbxm"));
  } finally {
    if (generated) {
      fs.rmSync(source, { force: true });
    }
  }
}

function syncProjectSchema() {
  for (const name of ["renium.project.schema.json", "renium.meta.schema.json"]) {
    const source = path.join(repoRoot, "tools", "renium", "schemas", name);
    fs.copyFileSync(source, path.join(extensionResources, name));
  }
}

function syncAgentInstructions() {
  fs.copyFileSync(
    path.join(repoRoot, "tools", "renium", "renium-agents.md"),
    path.join(extensionResources, "RENIUM.md"),
  );
  fs.rmSync(path.join(extensionResources, "AGENTS.md"), { force: true });
}

function discoverInsertableObjectsIconRoot() {
  const localAppData = process.env.LOCALAPPDATA;
  if (!localAppData) {
    return undefined;
  }
  const versionsDir = path.join(localAppData, "Roblox", "Versions");
  if (!fs.existsSync(versionsDir)) {
    return undefined;
  }
  let best;
  for (const entry of fs.readdirSync(versionsDir, { withFileTypes: true })) {
    if (!entry.isDirectory()) {
      continue;
    }
    const candidate = path.join(versionsDir, entry.name, "content", "studio_svg_textures", "Shared", "InsertableObjects");
    const themed = path.join(candidate, insertableObjectsIconTheme, insertableObjectsIconSize);
    if (!fs.existsSync(themed)) {
      continue;
    }
    const mtime = fs.statSync(themed).mtimeMs;
    if (!best || mtime > best.mtime) {
      best = { candidate, mtime };
    }
  }
  return best?.candidate;
}

const insertableObjectsIconRoot = process.env.RENIUM_INSERTABLE_OBJECTS_ICON_PATH ??
  discoverInsertableObjectsIconRoot();

function iconFileInfo(fileName) {
  if (!/\.png$/i.test(fileName)) {
    return undefined;
  }
  const name = path.basename(fileName, ".png");
  const scaled = name.match(/^(.*)(@[23]x)$/);
  if (scaled) {
    return { className: scaled[1], scale: scaled[2] };
  }
  return { className: name, scale: "" };
}

function iconScaleRank(scale) {
  if (scale === preferredIconScale) {
    return 0;
  }
  if (scale === "") {
    return 1;
  }
  if (scale === "@3x") {
    return 2;
  }
  return 3;
}

function insertableObjectIconFiles(sourceDirectory) {
  const icons = new Map();
  if (!fs.existsSync(sourceDirectory)) {
    return icons;
  }
  for (const entry of fs.readdirSync(sourceDirectory, { withFileTypes: true })) {
    if (!entry.isFile()) {
      continue;
    }
    const info = iconFileInfo(entry.name);
    if (!info) {
      continue;
    }
    const sourcePath = path.join(sourceDirectory, entry.name);
    const current = icons.get(info.className);
    if (!current || iconScaleRank(info.scale) < iconScaleRank(current.scale)) {
      icons.set(info.className, { sourcePath, scale: info.scale });
    }
  }
  return icons;
}

function syncInsertableObjectIcons() {
  if (!insertableObjectsIconRoot) {
    console.warn("Renium: no Roblox Studio install with class icons found; keeping existing bundled icons.");
    return;
  }
  const sourceDirectory = path.join(insertableObjectsIconRoot, insertableObjectsIconTheme, insertableObjectsIconSize);
  const icons = insertableObjectIconFiles(sourceDirectory);
  if (icons.size === 0) {
    console.warn(`Renium: no Roblox class icons found in ${sourceDirectory}; keeping existing bundled icons.`);
    return;
  }

  const allowedPngNames = new Set([
    ...logoFiles.map((fileName) => path.basename(fileName, ".png")),
    ...icons.keys(),
  ]);
  for (const entry of fs.readdirSync(extensionAssets, { withFileTypes: true })) {
    if (!entry.isFile()) {
      continue;
    }
    const info = iconFileInfo(entry.name);
    if (info && !allowedPngNames.has(info.className)) {
      fs.rmSync(path.join(extensionAssets, entry.name), { force: true });
    }
  }

  for (const [className, icon] of icons) {
    fs.copyFileSync(icon.sourcePath, path.join(extensionAssets, `${className}.png`));
  }
  console.log(`Synced ${icons.size} Roblox class icons from ${sourceDirectory} (${preferredIconScale} preferred).`);
}

fs.mkdirSync(extensionAssets, { recursive: true });
fs.mkdirSync(extensionResources, { recursive: true });

for (const fileName of logoFiles) {
  const asset = path.join(extensionAssets, fileName);
  if (!fs.existsSync(asset)) {
    throw new Error(`Missing extension logo asset: ${asset}`);
  }
}

if (refreshStudioApi || !fs.existsSync(path.join(extensionResources, "roblox-properties.generated.json"))) {
  generateRobloxPropertiesMetadata({ extensionRoot, repoRoot, refreshStudioApi });
}
syncStudioPluginBundle();
syncProjectSchema();
syncAgentInstructions();
syncInsertableObjectIcons();
