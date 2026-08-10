import * as childProcess from "child_process";
import * as path from "path";

type ProcessEntry = {
  child: childProcess.ChildProcess;
  owner: string;
  closed: Promise<void>;
  resolveClosed: () => void;
  closedSettled: boolean;
};

const entries = new Map<childProcess.ChildProcess, ProcessEntry>();

function finishEntry(entry: ProcessEntry): void {
  if (entry.closedSettled) {
    return;
  }
  entry.closedSettled = true;
  if (entries.get(entry.child) === entry) {
    entries.delete(entry.child);
  }
  entry.resolveClosed();
}

function ensureEntry(child: childProcess.ChildProcess, owner: string): ProcessEntry {
  const existing = entries.get(child);
  if (existing) {
    return existing;
  }
  let resolveClosed = (): void => undefined;
  const closed = new Promise<void>((resolve) => {
    resolveClosed = resolve;
  });
  const entry: ProcessEntry = {
    child,
    owner,
    closed,
    resolveClosed,
    closedSettled: false,
  };
  entries.set(child, entry);
  child.once("close", () => finishEntry(entry));
  if (
    (child.exitCode !== null || child.signalCode !== null)
    && [child.stdin, child.stdout, child.stderr].every((stream) => !stream || stream.closed || stream.destroyed)
  ) {
    finishEntry(entry);
  }
  return entry;
}

function waitBounded(promise: Promise<void>, timeoutMs: number): Promise<boolean> {
  return new Promise((resolve) => {
    let settled = false;
    const timer = setTimeout(() => {
      if (!settled) {
        settled = true;
        resolve(false);
      }
    }, timeoutMs);
    void promise.then(() => {
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        resolve(true);
      }
    });
  });
}

async function forceTerminateTree(child: childProcess.ChildProcess): Promise<void> {
  const pid = child.pid;
  if (!pid) {
    return;
  }
  if (process.platform === "win32") {
    await runTaskkill(pid, true);
    return;
  }
  await signalPosixTree(pid, "SIGKILL");
}

async function gracefullyTerminateTree(child: childProcess.ChildProcess): Promise<void> {
  const pid = child.pid;
  if (!pid) {
    return;
  }
  if (process.platform === "win32") {
    await runTaskkill(pid, false);
    return;
  }
  await signalPosixTree(pid, "SIGTERM");
}

async function runTaskkill(pid: number, force: boolean): Promise<void> {
  await new Promise<void>((resolve) => {
    let settled = false;
    const args = ["/PID", String(pid), "/T"];
    if (force) {
      args.push("/F");
    }
    const killer = childProcess.spawn("taskkill.exe", args, {
      shell: false,
      stdio: "ignore",
      windowsHide: true,
    });
    const finish = (): void => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(() => {
      killer.kill();
      finish();
    }, 2000);
    killer.once("close", finish);
    killer.once("error", finish);
  });
}

function posixDescendants(rootPid: number): Promise<number[]> {
  return new Promise<number[]>((resolve) => {
    let output = "";
    let settled = false;
    const ps = childProcess.spawn("ps", ["-eo", "pid=,ppid="], {
      shell: false,
      stdio: ["ignore", "pipe", "ignore"],
    });
    ps.stdout?.on("data", (chunk: Buffer | string) => {
      output += chunk.toString();
    });
    const finish = (): void => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      const children = new Map<number, number[]>();
      for (const line of output.split(/\r?\n/)) {
        const [pidText, parentText] = line.trim().split(/\s+/);
        const pid = Number(pidText);
        const parent = Number(parentText);
        if (!Number.isInteger(pid) || !Number.isInteger(parent)) {
          continue;
        }
        const values = children.get(parent) ?? [];
        values.push(pid);
        children.set(parent, values);
      }
      const descendants: number[] = [];
      const stack = [...(children.get(rootPid) ?? [])];
      while (stack.length > 0) {
        const pid = stack.pop();
        if (!pid) {
          continue;
        }
        descendants.push(pid);
        stack.push(...(children.get(pid) ?? []));
      }
      resolve(descendants.reverse());
    };
    const timer = setTimeout(() => {
      ps.kill();
      if (!settled) {
        settled = true;
        resolve([]);
      }
    }, 2000);
    ps.once("close", finish);
    ps.once("error", () => {
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        resolve([]);
      }
    });
  });
}

async function signalPosixTree(pid: number, signal: NodeJS.Signals): Promise<void> {
  const targets = [...await posixDescendants(pid), pid];
  for (const target of targets) {
    try {
      process.kill(-target, signal);
    } catch {
    }
    try {
      process.kill(target, signal);
    } catch {
    }
  }
}

export function projectProcessOwner(projectRoot: string): string {
  const normalized = path.resolve(projectRoot).replaceAll("\\", "/");
  return process.platform === "win32" ? normalized.toLowerCase() : normalized;
}

export function trackProcess(child: childProcess.ChildProcess, owner: string): Promise<void> {
  return ensureEntry(child, owner).closed;
}

export function spawnTrackedProcess(
  command: string,
  args: readonly string[],
  cwd: string,
): { child: childProcess.ChildProcessWithoutNullStreams; closed: Promise<void> } {
  const child = childProcess.spawn(command, args, {
    cwd,
    env: process.env,
    detached: process.platform !== "win32",
    shell: false,
    stdio: "pipe",
    windowsHide: true,
  });
  return { child, closed: trackProcess(child, projectProcessOwner(cwd)) };
}

export async function terminateProcess(child: childProcess.ChildProcess, gracefulMs = 750): Promise<void> {
  const entry = ensureEntry(child, "");
  if (entry.closedSettled) {
    return;
  }
  await gracefullyTerminateTree(child);
  if (await waitBounded(entry.closed, gracefulMs)) {
    return;
  }
  await forceTerminateTree(child);
  if (await waitBounded(entry.closed, 2000)) {
    return;
  }
  child.stdin?.destroy();
  child.stdout?.destroy();
  child.stderr?.destroy();
  finishEntry(entry);
}

export async function terminateProcessesForOwner(owner: string): Promise<void> {
  const owned = Array.from(entries.values())
    .filter((entry) => entry.owner === owner)
    .map((entry) => terminateProcess(entry.child));
  await Promise.allSettled(owned);
}

export async function terminateAllProcesses(): Promise<void> {
  await Promise.allSettled(Array.from(entries.values()).map((entry) => terminateProcess(entry.child)));
}
