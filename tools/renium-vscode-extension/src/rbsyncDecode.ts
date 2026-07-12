
import * as childProcess from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";

export type DecodeResult = { ok: true; tree: unknown } | { ok: false; error: string };

/**
 * These limits keep a malformed or unexpectedly large store from monopolizing
 * the extension host. The actual binary decoding still happens in the CLI,
 * outside VS Code's renderer process.
 */
export const MAX_RBSYNC_INPUT_BYTES = 64 * 1024 * 1024;
export const MAX_RBSYNC_DROPPED_BYTES = 16 * 1024 * 1024;
export const MAX_RBSYNC_VIEW_OUTPUT_BYTES = 32 * 1024 * 1024;
export const RBSYNC_DECODE_TIMEOUT_MS = 60_000;

const MAX_RBSYNC_ERROR_BYTES = 64 * 1024;
const MAX_RBSYNC_TREE_NODES = 100_000;
const MAX_RBSYNC_TREE_DEPTH = 512;

type RbsyncDecodeOptions = {
  maxInputBytes?: number;
  maxOutputBytes?: number;
  timeoutMs?: number;
};

type RbsyncTreeNode = {
  children?: unknown;
};

function boundedLimit(value: number | undefined, fallback: number, maximum: number): number {
  const numeric = Number(value);
  if (!Number.isFinite(numeric)) {
    return fallback;
  }
  return Math.max(1, Math.min(maximum, Math.floor(numeric)));
}

function appendLimited(
  chunks: Buffer[],
  currentLength: number,
  data: Buffer | string,
  limit: number,
): { length: number; exceeded: boolean } {
  const chunk = Buffer.isBuffer(data) ? data : Buffer.from(data);
  if (currentLength + chunk.length > limit) {
    return { length: currentLength, exceeded: true };
  }
  chunks.push(chunk);
  return { length: currentLength + chunk.length, exceeded: false };
}

function cliFailureMessage(stdout: Buffer[], stderr: Buffer[], code: number | null): string {
  const message = Buffer.concat(stderr).toString("utf8").trim()
    || Buffer.concat(stdout).toString("utf8").trim()
    || `renium view exited with code ${code ?? "unknown"}.`;
  return message.length > MAX_RBSYNC_ERROR_BYTES
    ? `${message.slice(0, MAX_RBSYNC_ERROR_BYTES)}\n[Renium truncated this error.]`
    : message;
}

function validateTree(tree: unknown): string | undefined {
  if (!tree || typeof tree !== "object" || Array.isArray(tree)) {
    return "Renium view returned JSON without a tree object.";
  }
  const roots = (tree as { roots?: unknown }).roots;
  if (!Array.isArray(roots)) {
    return "Renium view returned a tree without roots.";
  }

  let count = 0;
  const pending: Array<{ node: unknown; depth: number }> = roots.map((node) => ({ node, depth: 0 }));
  while (pending.length > 0) {
    const current = pending.pop()!;
    if (!current.node || typeof current.node !== "object" || Array.isArray(current.node)) {
      return "Renium view returned an invalid tree node.";
    }
    count += 1;
    if (count > MAX_RBSYNC_TREE_NODES) {
      return `This store has more than ${MAX_RBSYNC_TREE_NODES.toLocaleString()} instances, which exceeds the viewer safety limit.`;
    }
    if (current.depth > MAX_RBSYNC_TREE_DEPTH) {
      return `This store is nested more than ${MAX_RBSYNC_TREE_DEPTH} levels deep, which exceeds the viewer safety limit.`;
    }
    const children = (current.node as RbsyncTreeNode).children;
    if (children === undefined) {
      continue;
    }
    if (!Array.isArray(children)) {
      return "Renium view returned an invalid children list.";
    }
    for (const child of children) {
      pending.push({ node: child, depth: current.depth + 1 });
    }
  }
  return undefined;
}

async function runViewCommand(
  cliPath: string,
  cwd: string,
  filePath: string,
  maxOutputBytes: number,
  timeoutMs: number,
): Promise<DecodeResult> {
  return await new Promise<DecodeResult>((resolve) => {
    let settled = false;
    let timeout: NodeJS.Timeout | undefined;
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    let stdoutLength = 0;
    let stderrLength = 0;

    const finish = (result: DecodeResult): void => {
      if (settled) {
        return;
      }
      settled = true;
      if (timeout) {
        clearTimeout(timeout);
      }
      resolve(result);
    };

    let child: childProcess.ChildProcess;
    try {
      child = childProcess.spawn(cliPath, ["view", filePath, "--json"], {
        cwd,
        env: process.env,
        shell: false,
        stdio: "pipe",
        windowsHide: true,
      });
    } catch (err) {
      finish({ ok: false, error: err instanceof Error ? err.message : String(err) });
      return;
    }

    const stop = (error: string): void => {
      try {
        child.kill();
      } catch {
      }
      finish({ ok: false, error });
    };

    timeout = setTimeout(() => {
      stop(`Decoding timed out after ${Math.round(timeoutMs / 1000)} seconds.`);
    }, timeoutMs);

    child.stdout?.on("data", (data: Buffer | string) => {
      if (settled) {
        return;
      }
      const next = appendLimited(stdout, stdoutLength, data, maxOutputBytes);
      stdoutLength = next.length;
      if (next.exceeded) {
        stop(`Decoded tree exceeds the ${Math.floor(maxOutputBytes / (1024 * 1024))} MiB viewer limit.`);
      }
    });
    child.stderr?.on("data", (data: Buffer | string) => {
      if (settled) {
        return;
      }
      const next = appendLimited(stderr, stderrLength, data, MAX_RBSYNC_ERROR_BYTES);
      stderrLength = next.length;
    });
    child.once("error", (err) => {
      finish({ ok: false, error: err.message });
    });
    child.once("close", (code) => {
      if (settled) {
        return;
      }
      if (code !== 0) {
        finish({ ok: false, error: cliFailureMessage(stdout, stderr, code) });
        return;
      }
      try {
        const tree = JSON.parse(Buffer.concat(stdout).toString("utf8")) as unknown;
        const treeError = validateTree(tree);
        finish(treeError ? { ok: false, error: treeError } : { ok: true, tree });
      } catch (err) {
        finish({ ok: false, error: err instanceof Error ? err.message : String(err) });
      }
    });
  });
}

/** Decode a .renium file to the viewer's JSON tree via `renium view --json`. */
export async function decodeRbsyncToTree(
  cliPath: string,
  cwd: string,
  filePath: string,
  options: RbsyncDecodeOptions = {},
): Promise<DecodeResult> {
  const maxInputBytes = boundedLimit(options.maxInputBytes, MAX_RBSYNC_INPUT_BYTES, MAX_RBSYNC_INPUT_BYTES);
  const maxOutputBytes = boundedLimit(options.maxOutputBytes, MAX_RBSYNC_VIEW_OUTPUT_BYTES, MAX_RBSYNC_VIEW_OUTPUT_BYTES);
  const timeoutMs = boundedLimit(options.timeoutMs, RBSYNC_DECODE_TIMEOUT_MS, RBSYNC_DECODE_TIMEOUT_MS);
  try {
    const stat = await fs.promises.stat(filePath);
    if (!stat.isFile()) {
      return { ok: false, error: "The selected path is not a regular file." };
    }
    if (stat.size > maxInputBytes) {
      return {
        ok: false,
        error: `This .renium file is ${Math.ceil(stat.size / (1024 * 1024))} MiB; the viewer limit is ${Math.floor(maxInputBytes / (1024 * 1024))} MiB.`,
      };
    }
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : String(err) };
  }
  return await runViewCommand(cliPath, cwd, filePath, maxOutputBytes, timeoutMs);
}

/** Decode raw bytes (a dropped file) by round-tripping through a private temp
 * directory so the same file-based CLI path handles both drops and opened files. */
export async function decodeRbsyncBytes(
  cliPath: string,
  cwd: string,
  bytes: Buffer,
): Promise<DecodeResult> {
  if (bytes.length > MAX_RBSYNC_DROPPED_BYTES) {
    return {
      ok: false,
      error: `Dropped files are limited to ${Math.floor(MAX_RBSYNC_DROPPED_BYTES / (1024 * 1024))} MiB. Use the file picker for a larger store.`,
    };
  }
  let tempDir: string | undefined;
  try {
    tempDir = await fs.promises.mkdtemp(path.join(os.tmpdir(), "renium-view-"));
    const tempFile = path.join(tempDir, "dropped.renium");
    await fs.promises.writeFile(tempFile, bytes, { flag: "wx" });
    return await decodeRbsyncToTree(cliPath, cwd, tempFile, { maxInputBytes: MAX_RBSYNC_DROPPED_BYTES });
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : String(err) };
  } finally {
    if (tempDir) {
      try {
        await fs.promises.rm(tempDir, { recursive: true, force: true });
      } catch {
      }
    }
  }
}
