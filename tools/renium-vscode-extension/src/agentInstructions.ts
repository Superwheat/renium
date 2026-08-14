import * as fs from "fs";
import * as path from "path";

const PROJECT_MARKERS = [
  "renium.experience.json",
  "renium.project.json",
  "renium.project.jsonc",
  "sourcemap.json",
  "renium-link.json",
  ".renium",
] as const;

const INSTRUCTION_FILES = ["AGENTS.md", "CLAUDE.md"] as const;

export function isReniumProjectRoot(projectRoot: string): boolean {
  return PROJECT_MARKERS.some((name) => fs.existsSync(path.join(projectRoot, name)));
}

export function ensureReniumAgentInstructions(
  extensionRoot: string,
  projectRoot: string,
): string[] {
  const created: string[] = [];
  if (!fs.existsSync(projectRoot) || !fs.statSync(projectRoot).isDirectory()) {
    return created;
  }
  for (const name of INSTRUCTION_FILES) {
    const target = path.join(projectRoot, name);
    if (fs.existsSync(target)) {
      continue;
    }
    const packagedSource = path.join(extensionRoot, "resources", name);
    const source = name === "AGENTS.md" && !fs.existsSync(packagedSource)
      ? path.resolve(extensionRoot, "..", "renium", "renium-agents.md")
      : packagedSource;
    try {
      fs.copyFileSync(source, target, fs.constants.COPYFILE_EXCL);
      created.push(target);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") {
        throw error;
      }
    }
  }
  return created;
}
