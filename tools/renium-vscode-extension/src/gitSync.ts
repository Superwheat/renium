import * as childProcess from "child_process";
import * as path from "path";
import {
  projectProcessOwner,
  terminateProcess,
  trackProcess,
} from "./processSupervisor";

export type GitRunResult = {
  code: number;
  stdout: string;
  stderr: string;
  output: string;
  cwd: string;
  args: string[];
  timedOut: boolean;
};

export type GitRunOptions = {
  cwd: string;
  gitPath?: string;
  timeoutMs?: number;
  owner?: string;
  onOutput?: (stream: "stdout" | "stderr", text: string) => void;
};

export type GitFileKind =
  | "added"
  | "copied"
  | "conflicted"
  | "deleted"
  | "ignored"
  | "modified"
  | "renamed"
  | "typechange"
  | "unknown"
  | "untracked";

export type GitStatusEntry = {
  path: string;
  originalPath?: string;
  index: string;
  worktree: string;
  kind: GitFileKind;
  staged: boolean;
  unstaged: boolean;
  tracked: boolean;
  untracked: boolean;
  ignored: boolean;
  conflicted: boolean;
  deleted: boolean;
  raw: string;
};

export type GitNameStatusEntry = {
  status: string;
  path: string;
  originalPath?: string;
};

export function nameStatusAffectedPaths(entries: GitNameStatusEntry[]): string[] {
  const paths: string[] = [];
  const seen = new Set<string>();
  const push = (value: string | undefined): void => {
    if (!value || seen.has(value)) {
      return;
    }
    seen.add(value);
    paths.push(value);
  };

  for (const entry of entries) {
    if (entry.status.startsWith("R")) {
      push(entry.originalPath);
    }
    push(entry.path);
  }
  return paths;
}

export type GitStatusCounts = {
  total: number;
  tracked: number;
  staged: number;
  unstaged: number;
  untracked: number;
  ignored: number;
  conflicted: number;
  deleted: number;
};

const UNMERGED_STATUS_PAIRS = new Set(["DD", "AU", "UD", "UA", "DU", "AA", "UU"]);

export async function runGit(args: string[], options: GitRunOptions): Promise<GitRunResult> {
  const gitPath = options.gitPath?.trim() || "git";
  const timeoutMs = Math.max(1, Math.floor(options.timeoutMs ?? 120_000));
  const outputLimit = 16 * 1024 * 1024;

  return await new Promise<GitRunResult>((resolve, reject) => {
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    let outputLimitExceeded = false;
    let outputBytes = 0;
    let settled = false;
    let timer: NodeJS.Timeout | undefined;

    const child = childProcess.spawn(gitPath, args, {
      cwd: options.cwd,
      env: process.env,
      shell: false,
      stdio: "pipe",
      windowsHide: true,
      detached: true,
    });
    const processClosed = trackProcess(child, options.owner ?? projectProcessOwner(options.cwd));

    const finish = (code: number): void => {
      if (settled) {
        return;
      }
      settled = true;
      if (timer) {
        clearTimeout(timer);
      }
      resolve({
        code,
        stdout,
        stderr,
        output: `${stdout}${stderr}`,
        cwd: options.cwd,
        args,
        timedOut,
      });
    };

    timer = setTimeout(() => {
      timedOut = true;
      void terminateProcess(child).then(() => finish(124));
    }, timeoutMs);

    child.stdout?.on("data", (chunk: Buffer | string) => {
      const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
      const remaining = Math.max(0, outputLimit - outputBytes);
      const text = buffer.subarray(0, remaining).toString();
      outputBytes += buffer.length;
      stdout += text;
      options.onOutput?.("stdout", text);
      if (buffer.length > remaining && !outputLimitExceeded) {
        outputLimitExceeded = true;
        void terminateProcess(child).then(() => finish(125));
      }
    });

    child.stderr?.on("data", (chunk: Buffer | string) => {
      const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
      const remaining = Math.max(0, outputLimit - outputBytes);
      const text = buffer.subarray(0, remaining).toString();
      outputBytes += buffer.length;
      stderr += text;
      options.onOutput?.("stderr", text);
      if (buffer.length > remaining && !outputLimitExceeded) {
        outputLimitExceeded = true;
        void terminateProcess(child).then(() => finish(125));
      }
    });

    child.on("error", (err) => {
      if (settled) {
        return;
      }
      settled = true;
      if (timer) {
        clearTimeout(timer);
      }
      reject(err);
    });

    child.on("close", (code) => {
      finish(outputLimitExceeded ? 125 : code ?? (timedOut ? 124 : 130));
    });
    child.on("exit", (code) => {
      const resultCode = outputLimitExceeded ? 125 : code ?? (timedOut ? 124 : 130);
      void Promise.race([
        processClosed.then(() => true),
        new Promise<boolean>((resolveClose) => setTimeout(() => resolveClose(false), 100)),
      ]).then(async (closed) => {
        if (!closed) {
          await terminateProcess(child, 250);
        }
        finish(resultCode);
      });
    });
  });
}

export function redactRemoteUrl(value: string): string {
  return String(value ?? "")
    .replace(/(https?:\/\/)([^\s/@]+(?::[^\s/@]*)?@)/gi, "$1***@")
    .replace(/([?&](?:access_token|auth|password|token)=)[^&\s]+/gi, "$1***")
    .replace(/\b(?:ghp|gho|ghu|ghs|ghr|github_pat)_[A-Za-z0-9_]+\b/g, "***")
    .replace(/\bx-access-token:[^@\s]+@/gi, "x-access-token:***@");
}

export function renderGitArgs(args: string[]): string {
  return args
    .map((arg) => {
      const redacted = redactRemoteUrl(arg);
      if (/\s/.test(redacted) || redacted.includes('"')) {
        return `"${redacted.replaceAll('"', '\\"')}"`;
      }
      return redacted;
    })
    .join(" ");
}

export function parsePorcelainV1Z(raw: string): GitStatusEntry[] {
  if (!raw) {
    return [];
  }
  const tokens = raw.split("\0").filter((token) => token.length > 0);
  const entries: GitStatusEntry[] = [];

  for (let index = 0; index < tokens.length; index += 1) {
    const token = tokens[index];
    if (token.length < 3) {
      continue;
    }

    const indexStatus = token[0] ?? " ";
    const worktreeStatus = token[1] ?? " ";
    let filePath = token.slice(3);
    let originalPath: string | undefined;

    if ((indexStatus === "R" || indexStatus === "C") && index + 1 < tokens.length) {
      originalPath = tokens[index + 1];
      index += 1;
    } else if (filePath.includes(" -> ")) {
      const [from, to] = filePath.split(" -> ");
      originalPath = from;
      filePath = to ?? filePath;
    }

    const pair = `${indexStatus}${worktreeStatus}`;
    const untracked = pair === "??";
    const ignored = pair === "!!";
    const conflicted = UNMERGED_STATUS_PAIRS.has(pair) || indexStatus === "U" || worktreeStatus === "U";
    const staged = !untracked && !ignored && indexStatus !== " ";
    const unstaged = !untracked && !ignored && worktreeStatus !== " ";
    const deleted = indexStatus === "D" || worktreeStatus === "D";
    const kind = classifyStatus(indexStatus, worktreeStatus, untracked, ignored, conflicted);

    entries.push({
      path: filePath,
      originalPath,
      index: indexStatus,
      worktree: worktreeStatus,
      kind,
      staged,
      unstaged,
      tracked: !untracked && !ignored,
      untracked,
      ignored,
      conflicted,
      deleted,
      raw: token,
    });
  }

  return entries;
}

export function parseNameStatusZ(raw: string): GitNameStatusEntry[] {
  if (!raw) {
    return [];
  }
  const tokens = raw.split("\0").filter((token) => token.length > 0);
  const entries: GitNameStatusEntry[] = [];

  for (let index = 0; index < tokens.length; index += 1) {
    const status = tokens[index];
    const filePath = tokens[index + 1];
    if (!status || !filePath) {
      break;
    }
    index += 1;
    if ((status.startsWith("R") || status.startsWith("C")) && index + 1 < tokens.length) {
      const newPath = tokens[index + 1];
      entries.push({ status, originalPath: filePath, path: newPath });
      index += 1;
    } else {
      entries.push({ status, path: filePath });
    }
  }

  return entries;
}

export function parseAheadBehind(raw: string): { ahead: number; behind: number } {
  const [aheadRaw, behindRaw] = raw.trim().split(/\s+/);
  const ahead = Number.parseInt(aheadRaw ?? "0", 10);
  const behind = Number.parseInt(behindRaw ?? "0", 10);
  return {
    ahead: Number.isFinite(ahead) ? ahead : 0,
    behind: Number.isFinite(behind) ? behind : 0,
  };
}

export function summarizeStatus(entries: GitStatusEntry[]): GitStatusCounts {
  return {
    total: entries.length,
    tracked: entries.filter((entry) => entry.tracked).length,
    staged: entries.filter((entry) => entry.staged).length,
    unstaged: entries.filter((entry) => entry.unstaged).length,
    untracked: entries.filter((entry) => entry.untracked).length,
    ignored: entries.filter((entry) => entry.ignored).length,
    conflicted: entries.filter((entry) => entry.conflicted).length,
    deleted: entries.filter((entry) => entry.deleted).length,
  };
}

export function remoteUrlToWebUrl(remoteUrl: string): string | undefined {
  const trimmed = String(remoteUrl ?? "").trim();
  if (!trimmed) {
    return undefined;
  }

  const sshMatch = /^git@([^:]+):(.+?)(?:\.git)?$/i.exec(trimmed);
  if (sshMatch) {
    return `https://${sshMatch[1]}/${sshMatch[2].replace(/\.git$/i, "")}`;
  }

  const sshUrlMatch = /^ssh:\/\/git@([^/]+)\/(.+?)(?:\.git)?$/i.exec(trimmed);
  if (sshUrlMatch) {
    return `https://${sshUrlMatch[1]}/${sshUrlMatch[2].replace(/\.git$/i, "")}`;
  }

  const httpsMatch = /^(https?:\/\/[^\s]+?)(?:\.git)?$/i.exec(trimmed);
  if (httpsMatch) {
    try {
      const url = new URL(trimmed);
      url.username = "";
      url.password = "";
      url.hash = "";
      url.search = "";
      return url.toString().replace(/\.git$/i, "").replace(/\/$/, "");
    } catch {
      return httpsMatch[1]
        .replace(/^(https?:\/\/)([^@/]+@)/i, "$1")
        .replace(/\.git$/i, "");
    }
  }

  return undefined;
}

export function buildCommitMessage(template: string, branch: string): string {
  const now = new Date();
  const date = now.toISOString().slice(0, 10);
  const datetime = now.toISOString().replace(/\.\d{3}Z$/, "Z");
  return String(template || "Renium sync: ${date}")
    .replaceAll("${date}", date)
    .replaceAll("${datetime}", datetime)
    .replaceAll("${branch}", branch || "HEAD");
}

export function defaultGitSyncScope(repoRoot: string, projectRoot: string, srcDir = "src"): string {
  const srcRoot = path.join(projectRoot, srcDir);
  const relative = path.relative(repoRoot, srcRoot).replaceAll("\\", "/");
  return relative && relative !== "." ? relative : srcDir.replaceAll("\\", "/");
}

export function shouldPullFromStudioBeforePush(
  policy: "ask" | "always" | "never",
  forced: boolean,
  choice?: "pull" | "current",
): boolean {
  return forced || policy === "always" || (policy === "ask" && choice === "pull");
}

function classifyStatus(
  indexStatus: string,
  worktreeStatus: string,
  untracked: boolean,
  ignored: boolean,
  conflicted: boolean,
): GitFileKind {
  if (conflicted) {
    return "conflicted";
  }
  if (untracked) {
    return "untracked";
  }
  if (ignored) {
    return "ignored";
  }
  if (indexStatus === "R" || worktreeStatus === "R") {
    return "renamed";
  }
  if (indexStatus === "C" || worktreeStatus === "C") {
    return "copied";
  }
  if (indexStatus === "D" || worktreeStatus === "D") {
    return "deleted";
  }
  if (indexStatus === "A" || worktreeStatus === "A") {
    return "added";
  }
  if (indexStatus === "T" || worktreeStatus === "T") {
    return "typechange";
  }
  if (indexStatus === "M" || worktreeStatus === "M") {
    return "modified";
  }
  return "unknown";
}
