import * as fs from "fs";
import * as path from "path";

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
