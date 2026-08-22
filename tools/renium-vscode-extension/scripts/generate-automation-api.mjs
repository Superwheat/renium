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
  if (names.has(operation.name)) {
    throw new Error(`Duplicate operation name ${operation.name}`);
  }
  names.add(operation.name);
}

const propertyName = (name) => name.replace(/-([a-z])/g, (_, character) => character.toUpperCase());
const generatedTypeScript = [
  `export const AUTOMATION_PROTOCOL_VERSION = ${registry.version} as const;`,
  "",
  "export const AUTOMATION_OP = {",
  ...registry.operations.map((operation) => `  ${propertyName(operation.name)}: ${operation.id},`),
  "} as const;",
  "",
  "export const AUTOMATION_RUNTIME_OPS = new Set<number>([",
  ...registry.operations.filter((operation) => operation.runtime).map((operation) => `  ${operation.id},`),
  "]);",
  "",
].join("\n");
fs.writeFileSync(path.join(extensionRoot, "src", "automationProtocol.generated.ts"), generatedTypeScript);
