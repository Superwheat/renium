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
const GUIDE_VERSION = /^<!-- renium-version: ([^ ]+) -->$/;

function guideVersion(contents: Buffer): string | undefined {
  return contents.toString("utf8").split(/\r?\n/, 1)[0]?.match(GUIDE_VERSION)?.[1];
}

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
  force = false,
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
  const packagedTopics = path.join(extensionRoot, "resources", "RENIUM");
  const sourceRoot = path.resolve(extensionRoot, "..", "renium");
  const guideSource = fs.existsSync(packagedGuide)
    ? packagedGuide
    : path.join(sourceRoot, "renium-agents.md");
  const topicSource = fs.existsSync(packagedTopics)
    ? packagedTopics
    : path.join(sourceRoot, "renium-guides");
  const projectGuide = path.join(projectRoot, "RENIUM.md");
  const packagedGuideContents = fs.readFileSync(guideSource);
  const packagedVersion = guideVersion(packagedGuideContents);
  if (!packagedVersion) {
    throw new Error("The packaged RENIUM.md is missing its version marker.");
  }
  const deferUpdate = !force && fs.existsSync(projectGuide)
    && guideVersion(fs.readFileSync(projectGuide)) !== packagedVersion;
  if (!deferUpdate) {
    write(projectGuide, packagedGuideContents);
    const projectTopics = path.join(projectRoot, "RENIUM");
    fs.mkdirSync(projectTopics, { recursive: true });
    for (const name of fs.readdirSync(topicSource).filter((name) => name.endsWith(".md")).sort()) {
      write(path.join(projectTopics, name), fs.readFileSync(path.join(topicSource, name)));
    }
  }

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
