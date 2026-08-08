import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const [artifactRoot, version, repository, outputPath] = process.argv.slice(2);
if (!artifactRoot || !version || !repository || !outputPath) {
  throw new Error("Usage: node generate-update-manifest.mjs ARTIFACT_ROOT VERSION OWNER/REPO OUTPUT");
}

const privateKeyBase64 = process.env.RENIUM_UPDATE_PRIVATE_KEY?.trim();
const publicKeyBase64 = process.env.RENIUM_UPDATE_PUBLIC_KEY?.trim();
if (!privateKeyBase64 || !publicKeyBase64) {
  throw new Error("RENIUM_UPDATE_PRIVATE_KEY and RENIUM_UPDATE_PUBLIC_KEY are required");
}

function walk(directory) {
  const files = [];
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const entryPath = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      files.push(...walk(entryPath));
    } else if (entry.isFile()) {
      files.push(entryPath);
    }
  }
  return files;
}

const files = walk(artifactRoot);
const byName = new Map();
for (const file of files) {
  const name = path.basename(file);
  const entries = byName.get(name) ?? [];
  entries.push(file);
  byName.set(name, entries);
}

function one(name) {
  const matches = byName.get(name) ?? [];
  if (matches.length !== 1) {
    throw new Error(`Expected one ${name}, found ${matches.length}`);
  }
  return matches[0];
}

function artifact(name) {
  const file = one(name);
  return {
    url: `https://github.com/${repository}/releases/download/v${version}/${name}`,
    sha256: crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex"),
  };
}

const platforms = [
  ["linux-aarch64", `renium-${version}-linux-arm64.zip`, `renium-${version}-linux-arm64.vsix`, false],
  ["linux-x86_64", `renium-${version}-linux-x64.zip`, `renium-${version}-linux-x64.vsix`, false],
  ["macos-aarch64", `renium-${version}-macos-arm64.zip`, `renium-${version}-darwin-arm64.vsix`, true],
  ["macos-x86_64", `renium-${version}-macos-x64.zip`, `renium-${version}-darwin-x64.vsix`, true],
  ["windows-aarch64", `renium-${version}-windows-arm64.zip`, `renium-${version}-win32-arm64.vsix`, true],
  ["windows-x86_64", `renium-${version}-windows-x64.zip`, `renium-${version}-win32-x64.vsix`, true],
];
const components = {};
for (const [platform, coreName, extensionName, hasPlugin] of platforms) {
  components[platform] = {
    cli: artifact(coreName),
    plugin: hasPlugin ? artifact("Renium.rbxm") : null,
    extension: artifact(extensionName),
  };
}

const payload = {
  schemaVersion: 1,
  version,
  components,
};
const privateKey = crypto.createPrivateKey({
  key: Buffer.from(privateKeyBase64, "base64"),
  format: "der",
  type: "pkcs8",
});
if (privateKey.asymmetricKeyType !== "ed25519") {
  throw new Error("RENIUM_UPDATE_PRIVATE_KEY must be an Ed25519 PKCS#8 DER key");
}
const publicKey = crypto.createPublicKey(privateKey).export({ format: "der", type: "spki" });
const rawPublicKey = publicKey.subarray(publicKey.length - 32).toString("base64");
if (rawPublicKey !== publicKeyBase64) {
  throw new Error("RENIUM_UPDATE_PUBLIC_KEY does not match the signing key");
}
const signature = crypto.sign(null, Buffer.from(JSON.stringify(payload)), privateKey).toString("base64");
fs.writeFileSync(outputPath, `${JSON.stringify({ payload, signature }, null, 2)}\n`);
