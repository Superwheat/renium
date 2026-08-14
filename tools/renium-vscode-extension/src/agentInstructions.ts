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

const OLD_AGENT_MARKER = /^renium-\d+\.\d+\.\d+$/;

function agentInstructions(pointer: string, current = ""): string {
  if (current.includes(pointer.trimEnd())) {
    return current;
  }
  if (OLD_AGENT_MARKER.test(current.trimEnd().split(/\r?\n/).at(-1) ?? "")) {
    return pointer;
  }
  const separator = current.length === 0 || current.endsWith("\n\n")
    ? ""
    : current.endsWith("\n") ? "\n" : "\n\n";
  return `${current}${separator}${pointer}`;
}

export function isReniumProjectRoot(projectRoot: string): boolean {
  return PROJECT_MARKERS.some((name) => fs.existsSync(path.join(projectRoot, name)));
}

export function ensureReniumAgentInstructions(
  extensionRoot: string,
  projectRoot: string,
): string[] {
  if (!fs.existsSync(projectRoot)
    || !fs.statSync(projectRoot).isDirectory()
    || !isReniumProjectRoot(projectRoot)) {
    return [];
  }

  const written: string[] = [];
  const write = (target: string, contents: Buffer | string): void => {
    if (fs.existsSync(target) && fs.readFileSync(target).equals(Buffer.from(contents))) {
      return;
    }
    fs.writeFileSync(target, contents);
    written.push(target);
  };

  const packagedGuide = path.join(extensionRoot, "resources", "RENIUM.md");
  const guideSource = fs.existsSync(packagedGuide)
    ? packagedGuide
    : path.resolve(extensionRoot, "..", "renium", "renium-agents.md");
  write(path.join(projectRoot, "RENIUM.md"), fs.readFileSync(guideSource));

  const pointer = fs.readFileSync(path.join(extensionRoot, "resources", "RENIUM.pointer.md"), "utf8");
  for (const name of ["AGENTS.md", "CLAUDE.md"]) {
    const target = path.join(projectRoot, name);
    const current = fs.existsSync(target) ? fs.readFileSync(target, "utf8") : "";
    if (name === "CLAUDE.md" && current.toLowerCase().includes("agents.md")) {
      continue;
    }
    write(target, agentInstructions(pointer, current));
  }
  return written;
}
