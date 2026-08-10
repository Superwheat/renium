import * as fs from "fs";
import * as path from "path";

export function reniumBinaryName(platform: NodeJS.Platform = process.platform): string {
  return platform === "win32" ? "renium.exe" : "renium";
}

export function bundledReniumCliPath(
  extensionRoot: string,
  platform: NodeJS.Platform = process.platform,
  arch: string = process.arch,
): string {
  return path.join(extensionRoot, "bin", `${platform}-${arch}`, reniumBinaryName(platform));
}

export function findExecutableOnPath(
  binaryName: string,
  pathValue: string | undefined = process.env.PATH,
  platform: NodeJS.Platform = process.platform,
  pathExtValue: string | undefined = process.env.PATHEXT,
): string | undefined {
  if (!pathValue) {
    return undefined;
  }

  const hasExtension = path.extname(binaryName).length > 0;
  const extensions = platform === "win32" && !hasExtension
    ? (pathExtValue ?? ".COM;.EXE;.BAT;.CMD")
      .split(";")
      .map((value) => value.trim())
      .filter((value) => value.length > 0)
    : [""];

  for (const directory of pathValue.split(path.delimiter)) {
    const root = directory.length > 0 ? directory : process.cwd();
    for (const extension of extensions) {
      const candidate = path.resolve(root, `${binaryName}${extension}`);
      try {
        fs.accessSync(candidate, fs.constants.F_OK | fs.constants.X_OK);
        if (fs.statSync(candidate).isFile()) {
          return candidate;
        }
      } catch {
      }
    }
  }

  return undefined;
}

export function reniumCliCandidates(options: {
  configuredPath?: string;
  extensionRoot?: string;
  roots?: readonly string[];
  fallbackRelativePaths?: readonly string[];
  pathValue?: string;
  platform?: NodeJS.Platform;
  arch?: string;
  pathExtValue?: string;
}): string[] {
  const platform = options.platform ?? process.platform;
  const arch = options.arch ?? process.arch;
  const binaryName = reniumBinaryName(platform);
  const candidates: string[] = [];
  const configuredPath = options.configuredPath?.trim();
  if (configuredPath) {
    candidates.push(configuredPath);
  }
  if (options.extensionRoot) {
    candidates.push(bundledReniumCliPath(options.extensionRoot, platform, arch));
  }
  const pathCandidate = findExecutableOnPath(
    binaryName,
    options.pathValue,
    platform,
    options.pathExtValue,
  );
  if (pathCandidate) {
    candidates.push(pathCandidate);
  }
  for (const root of options.roots ?? []) {
    for (const relativePath of options.fallbackRelativePaths ?? []) {
      candidates.push(path.join(root, relativePath));
    }
  }

  const seen = new Set<string>();
  return candidates
    .map((candidate) => path.normalize(candidate))
    .filter((candidate) => {
      const key = platform === "win32" ? candidate.toLowerCase() : candidate;
      if (seen.has(key)) {
        return false;
      }
      seen.add(key);
      return true;
    });
}

function reniumCliFallbackRelativePaths(
  platform: NodeJS.Platform = process.platform,
): string[] {
  const binaryName = reniumBinaryName(platform);
  return [
    binaryName,
    `bin/${binaryName}`,
    `tools/renium/target/release/${binaryName}`,
    `tools/renium/target/debug/${binaryName}`,
  ];
}

export function resolveReniumCliPath(options: {
  configuredPath?: string;
  extensionRoot?: string;
  roots?: readonly string[];
  pathValue?: string;
  platform?: NodeJS.Platform;
  arch?: string;
  pathExtValue?: string;
}): string {
  const platform = options.platform ?? process.platform;
  const configuredPath = options.configuredPath?.trim();
  const candidates = reniumCliCandidates({
    ...options,
    configuredPath,
    fallbackRelativePaths: reniumCliFallbackRelativePaths(platform),
  });
  const existing = candidates.find((candidate) => {
    try {
      return fs.statSync(candidate).isFile();
    } catch {
      return false;
    }
  });
  if (existing) {
    return existing;
  }
  if (configuredPath) {
    return configuredPath;
  }
  if (options.extensionRoot) {
    return bundledReniumCliPath(
      options.extensionRoot,
      platform,
      options.arch ?? process.arch,
    );
  }
  return reniumBinaryName(platform);
}
