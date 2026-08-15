import * as childProcess from "child_process";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  projectProcessOwner,
  spawnTrackedProcess,
  terminateProcess,
  trackProcess,
} from "./processSupervisor";
import {
  AUTOMATION_OP,
  AUTOMATION_PROTOCOL_VERSION,
  AUTOMATION_RUNTIME_OPS,
} from "./automationProtocol.generated";
import { delay, prefixProcessOutput } from "./utils";

const DEFAULT_REQUEST_TIMEOUT_MS = 30 * 60 * 1000;
const MAX_OUTPUT_BUFFER_BYTES = 1024 * 1024;
const MAX_CHANNEL_WAIT_MS = 30_000;

type AutomationClientConfig = {
  projectRoot: string;
  placeSelector?: string;
  bridgePorts: string;
  bridgeWaitSeconds: number;
  progressHeartbeatSeconds: number;
};

type AutomationError = {
  c: string;
  m: string;
  rt: 0 | 1;
  n: string;
  d?: unknown;
};

export type CommandRunResult = {
  code: number;
  output: string;
  result?: unknown;
  automationError?: AutomationError;
};

type AutomationResponse = {
  v: typeof AUTOMATION_PROTOCOL_VERSION;
  id: number;
  ok: 0 | 1;
  ms: number;
  r?: unknown;
  e?: AutomationError;
};

type PendingRequest = {
  label: string;
  launchedAt: number;
  lastOutputAt: number;
  sawOutput: boolean;
  output: string;
  resolve: (result: CommandRunResult) => void;
  reject: (error: Error) => void;
  heartbeatTimer?: NodeJS.Timeout;
  timeoutTimer?: NodeJS.Timeout;
  quiet: boolean;
};

export function editorBridgeWaitSeconds(config: AutomationClientConfig): number {
  return Math.max(1, Math.min(MAX_CHANNEL_WAIT_MS / 1_000, Number(config.bridgeWaitSeconds) || 8));
}

function operationRequiresRuntime(op: number, parameters: Record<string, unknown>): boolean {
  return AUTOMATION_RUNTIME_OPS.has(op)
    || ((op === AUTOMATION_OP.setProperty || op === AUTOMATION_OP.remove) && parameters.editor === true);
}

export class AutomationClient {
  private process: childProcess.ChildProcessWithoutNullStreams | undefined;
  private processKey: string | undefined;
  private requestId = 1;
  private outputBuffer = "";
  private ready = false;
  private readyPromise: Promise<void> | undefined;
  private readyResolve: (() => void) | undefined;
  private readyReject: ((error: Error) => void) | undefined;
  private closePromise: Promise<void> | undefined;
  private stopPromise: Promise<void> | undefined;
  private pending = new Map<number, PendingRequest>();
  private context: { key: string; id: number } | undefined;

  public constructor(
    private readonly output: vscode.OutputChannel,
    private readonly ownerRoot: () => string,
  ) {}

  public isRunning(): boolean {
    return !!this.process
      && !this.process.killed
      && this.process.exitCode === null
      && this.process.signalCode === null;
  }

  public async runOperation(
    command: string,
    config: AutomationClientConfig,
    label: string,
    op: number,
    parameters: Record<string, unknown>,
    options: { quietWait?: boolean; timeoutMs?: number } = {},
  ): Promise<CommandRunResult> {
    await this.ensure(command, config);
    const contextId = await this.ensureContext(config, operationRequiresRuntime(op, parameters));
    return this.send(config, label, op, contextId, parameters, options);
  }

  public async runReviewedOperation(
    command: string,
    config: AutomationClientConfig,
    label: string,
    op: number,
    parameters: Record<string, unknown>,
    options: { quietWait?: boolean; timeoutMs?: number } = {},
  ): Promise<CommandRunResult> {
    await this.ensure(command, config);
    const contextId = await this.ensureContext(config, operationRequiresRuntime(op, parameters));
    const prepared = await this.send(
      config,
      `${label}-review`,
      AUTOMATION_OP.reviewPrepare,
      contextId,
      { op, p: parameters },
      { ...options, quietWait: true },
    );
    if (prepared.code !== 0) {
      return prepared;
    }
    const result = prepared.result as Record<string, unknown> | undefined;
    const reviewId = typeof result?.reviewId === "string" ? result.reviewId : undefined;
    if (!reviewId) {
      return { code: 1, output: "Review preparation did not return reviewId." };
    }
    return this.send(
      config,
      label,
      AUTOMATION_OP.reviewApply,
      contextId,
      { reviewId },
      options,
    );
  }

  public async ensure(
    command: string,
    config: AutomationClientConfig,
  ): Promise<void> {
    if (!fs.existsSync(command)) {
      throw new Error(`Required file not found: ${command}`);
    }
    const key = this.daemonKey(command, config);
    if (this.isRunning() && this.processKey === key) {
      await this.awaitReady(config);
      return;
    }
    await this.stop();
    const { child, closed } = spawnTrackedProcess(command, [
      "bd",
      "-w",
      String(Math.max(1, config.bridgeWaitSeconds)),
      "-P",
      config.bridgePorts,
      "--parent-pid",
      String(process.pid),
      "--editor-stdio",
    ], config.projectRoot);
    this.closePromise = closed;
    this.process = child;
    this.processKey = key;
    this.outputBuffer = "";
    this.ready = false;
    this.readyPromise = new Promise<void>((resolve, reject) => {
      this.readyResolve = resolve;
      this.readyReject = reject;
    });
    child.once("spawn", () => {
      this.ready = true;
      this.readyResolve?.();
      this.readyResolve = undefined;
      this.readyReject = undefined;
    });
    child.stdout.on("data", (data: Buffer | string) => this.handleOutput(command, data, false));
    child.stderr.on("data", (data: Buffer | string) => this.handleOutput(`${command}:err`, data, true));
    child.on("error", (error: Error) => {
      this.output.appendLine(`[renium] bridge daemon error: ${error.message}`);
      this.readyReject?.(error);
      this.rejectPending(error);
    });
    child.on("exit", (code: number | null) => {
      const error = new Error(`Persistent bridge daemon exited with code ${code ?? 0}`);
      if (code !== 0 && code !== null) {
        this.output.appendLine(`[renium] bridge daemon exited code=${code}`);
      }
      this.readyReject?.(error);
      this.rejectPending(error);
    });
    child.on("close", () => this.clearProcess(child));
    await this.awaitReady(config);
  }

  public stop(reason = new Error("Persistent bridge daemon was stopped.")): Promise<void> {
    if (this.stopPromise) {
      return this.stopPromise;
    }
    const stop = this.stopProcess(reason);
    const tracked = stop.finally(() => {
      if (this.stopPromise === tracked) {
        this.stopPromise = undefined;
      }
    });
    this.stopPromise = tracked;
    return tracked;
  }

  private daemonKey(command: string, config: AutomationClientConfig): string {
    let binaryMtimeMs = 0;
    try {
      binaryMtimeMs = Math.floor(fs.statSync(command).mtimeMs);
    } catch {
    }
    return JSON.stringify({
      command,
      binaryMtimeMs,
      projectRoot: config.projectRoot,
      bridgePorts: config.bridgePorts,
      bridgeWaitSeconds: Math.max(1, config.bridgeWaitSeconds),
    });
  }

  private async ensureContext(config: AutomationClientConfig, requireRuntime: boolean): Promise<number> {
    const key = JSON.stringify({
      projectRoot: path.resolve(config.projectRoot),
      place: config.placeSelector ?? "",
      daemon: this.processKey ?? "",
    });
    if (this.context?.key === key) {
      return this.context.id;
    }
    const deadline = Date.now() + editorBridgeWaitSeconds(config) * 1_000;
    while (true) {
      const bound = await this.send(
        config,
        "bind",
        AUTOMATION_OP.bind,
        undefined,
        { root: config.projectRoot, place: config.placeSelector },
        { quietWait: true, timeoutMs: 2_000 },
      );
      if (bound.code !== 0) {
        throw new Error(bound.automationError?.m ?? "Renium could not bind this project.");
      }
      const result = bound.result as Record<string, unknown> | undefined;
      const id = Number(result?.id);
      if (!Number.isSafeInteger(id) || id < 1) {
        throw new Error("Renium bind response omitted the context ID.");
      }
      if (typeof result?.runtimeId === "string" && result.runtimeId.length > 0) {
        this.context = { key, id };
        return id;
      }
      if (!requireRuntime) {
        return id;
      }
      let runtimeConnected = false;
      while (Date.now() < deadline) {
        const status = await this.send(
          config,
          "studios",
          AUTOMATION_OP.studios,
          id,
          {},
          { quietWait: true, timeoutMs: 1_000 },
        );
        if (status.code !== 0) {
          break;
        }
        const clients = (status.result as Record<string, unknown> | undefined)?.clients;
        if (Array.isArray(clients) && clients.length > 0) {
          runtimeConnected = true;
          break;
        }
        await delay(50);
      }
      await this.send(
        config,
        "unbind",
        AUTOMATION_OP.unbind,
        id,
        {},
        { quietWait: true, timeoutMs: 1_000 },
      );
      if (!runtimeConnected) {
        throw new Error("No Studio runtime is connected to this project.");
      }
    }
  }

  private send(
    config: AutomationClientConfig,
    label: string,
    op: number,
    contextId: number | undefined,
    parameters: Record<string, unknown>,
    options: { quietWait?: boolean; timeoutMs?: number } = {},
  ): Promise<CommandRunResult> {
    return new Promise<CommandRunResult>((resolve, reject) => {
      const proc = this.process;
      if (!proc || proc.killed || !proc.stdin.writable) {
        reject(new Error("Persistent bridge daemon is not running."));
        return;
      }
      const launchedAt = Date.now();
      const id = this.requestId++;
      const pending: PendingRequest = {
        label,
        launchedAt,
        lastOutputAt: launchedAt,
        sawOutput: false,
        output: "",
        resolve,
        reject,
        quiet: options.quietWait === true,
      };
      if (!pending.quiet) {
        const heartbeatMs = Math.max(2, Math.round(config.progressHeartbeatSeconds)) * 1000;
        pending.heartbeatTimer = setInterval(() => {
          const now = Date.now();
          const elapsedSec = ((now - launchedAt) / 1000).toFixed(1);
          const idleSec = ((now - pending.lastOutputAt) / 1000).toFixed(1);
          this.output.appendLine(
            pending.sawOutput
              ? `[renium] ${label}: daemon still running (${elapsedSec}s elapsed, idle ${idleSec}s)`
              : `[renium] ${label}: waiting for daemon output (${elapsedSec}s elapsed)`,
          );
        }, heartbeatMs);
      }
      const timeoutMs = this.requestTimeoutMs(config, op, options.timeoutMs);
      pending.timeoutTimer = setTimeout(() => {
        if (!this.pending.has(id)) {
          return;
        }
        const message = `[renium] ${label}: daemon request timed out after ${Math.round(timeoutMs / 1000)}s; restarting the bridge daemon.\n`;
        this.output.appendLine(message.trim());
        this.finishRequest(id, { code: 124, output: pending.output + `\n${message}` });
        void this.stop(new Error(`Persistent bridge daemon request timed out (${label}).`));
      }, timeoutMs);
      this.pending.set(id, pending);
      const request = `${JSON.stringify({
        v: AUTOMATION_PROTOCOL_VERSION,
        id,
        op,
        ...(contextId === undefined ? {} : { cx: contextId }),
        p: parameters,
      })}\n`;
      const writeFailed = (): void => {
        if (this.pending.has(id)) {
          this.finishRequest(id, {
            code: 1,
            output: `${pending.output}\nThe daemon transport closed before the request was written.`,
          });
        }
        void this.stop(new Error("Persistent bridge daemon transport closed."));
      };
      try {
        proc.stdin.write(request, "utf8", (error) => {
          if (error) {
            writeFailed();
          }
        });
      } catch {
        writeFailed();
      }
    });
  }

  private requestTimeoutMs(
    config: AutomationClientConfig,
    op: number,
    requestedTimeoutMs?: number,
  ): number {
    if (op === AUTOMATION_OP.liveStatus) {
      const bridgeWaitMs = (Math.max(1, Number(config.bridgeWaitSeconds) || 1) + 3) * 1000;
      return Math.max(5_000, Math.min(MAX_CHANNEL_WAIT_MS, bridgeWaitMs));
    }
    return Math.max(
      1_000,
      Math.min(DEFAULT_REQUEST_TIMEOUT_MS, Math.floor(Number(requestedTimeoutMs) || DEFAULT_REQUEST_TIMEOUT_MS)),
    );
  }

  private async awaitReady(config: AutomationClientConfig): Promise<void> {
    if (this.ready) {
      return;
    }
    const readyPromise = this.readyPromise;
    if (!readyPromise) {
      throw new Error("Persistent bridge daemon was not started.");
    }
    const timeoutMs = Math.max(
      1_000,
      Math.min(
        MAX_CHANNEL_WAIT_MS,
        (Math.max(1, Number(config.bridgeWaitSeconds) || 1) + 2) * 1000,
      ),
    );
    let timer: NodeJS.Timeout | undefined;
    try {
      await Promise.race([
        readyPromise,
        new Promise<void>((_resolve, reject) => {
          timer = setTimeout(
            () => reject(
              new Error(`Persistent bridge daemon did not become ready within ${Math.round(timeoutMs / 1000)}s.`),
            ),
            timeoutMs,
          );
        }),
      ]);
    } catch (error) {
      const reason = error instanceof Error ? error : new Error(String(error));
      await this.stop(reason);
      throw reason;
    } finally {
      if (timer) {
        clearTimeout(timer);
      }
    }
  }

  private handleOutput(prefix: string, data: Buffer | string, isStderr: boolean): void {
    const text = data.toString();
    if (isStderr && !Array.from(this.pending.values()).some((pending) => pending.quiet)) {
      this.output.append(prefixProcessOutput(prefix, data));
    }
    if (isStderr) {
      this.appendOutputToActiveRequest(text);
      return;
    }
    this.outputBuffer += text;
    if (this.outputBuffer.length > MAX_OUTPUT_BUFFER_BYTES) {
      const error = new Error("Persistent bridge daemon emitted more than 1 MiB without a complete protocol line.");
      this.output.appendLine(`[renium] bridge daemon protocol error: ${error.message}`);
      void this.stop(error);
      return;
    }
    let newlineIndex = this.outputBuffer.indexOf("\n");
    while (newlineIndex >= 0) {
      const line = this.outputBuffer.slice(0, newlineIndex).replace(/\r$/, "");
      this.outputBuffer = this.outputBuffer.slice(newlineIndex + 1);
      this.processLine(line);
      newlineIndex = this.outputBuffer.indexOf("\n");
    }
  }

  private appendOutputToActiveRequest(text: string): void {
    const active = this.pending.values().next().value as PendingRequest | undefined;
    if (!active) {
      return;
    }
    active.output += text;
    if (active.output.length > 8_000_000) {
      active.output = active.output.slice(-8_000_000);
    }
    active.sawOutput = true;
    active.lastOutputAt = Date.now();
  }

  private processLine(line: string): void {
    let payload: AutomationResponse;
    try {
      payload = JSON.parse(line) as AutomationResponse;
    } catch (error) {
      this.output.appendLine(
        `[renium] bridge daemon returned invalid protocol JSON: ${error instanceof Error ? error.message : String(error)}`,
      );
      return;
    }
    if (payload.v !== AUTOMATION_PROTOCOL_VERSION || (payload.ok !== 0 && payload.ok !== 1)) {
      this.output.appendLine("[renium] bridge daemon returned an incompatible protocol response.");
      return;
    }
    const id = Number(payload.id ?? 0);
    const pending = this.pending.get(id);
    if (!pending) {
      return;
    }
    if (payload.e?.c === "stale_cx") {
      this.context = undefined;
    }
    let output = pending.output;
    if (payload.e?.m) {
      output += `\n${payload.e.m}\n`;
    }
    this.finishRequest(id, {
      code: payload.ok === 1 ? 0 : 1,
      output,
      result: payload.r,
      automationError: payload.e,
    });
  }

  private finishRequest(id: number, result: CommandRunResult): void {
    const pending = this.pending.get(id);
    if (!pending) {
      return;
    }
    if (pending.heartbeatTimer) {
      clearInterval(pending.heartbeatTimer);
    }
    if (pending.timeoutTimer) {
      clearTimeout(pending.timeoutTimer);
    }
    this.pending.delete(id);
    if (!pending.quiet) {
      const elapsedSec = ((Date.now() - pending.launchedAt) / 1000).toFixed(1);
      this.output.appendLine(`[renium] ${pending.label}: daemon result code=${result.code} after ${elapsedSec}s`);
    }
    pending.resolve(result);
  }

  private rejectPending(error: Error): void {
    for (const [id, pending] of this.pending) {
      if (pending.heartbeatTimer) {
        clearInterval(pending.heartbeatTimer);
      }
      if (pending.timeoutTimer) {
        clearTimeout(pending.timeoutTimer);
      }
      this.pending.delete(id);
      pending.reject(error);
    }
  }

  private clearProcess(process: childProcess.ChildProcess): void {
    if (this.process !== process) {
      return;
    }
    this.process = undefined;
    this.processKey = undefined;
    this.outputBuffer = "";
    this.ready = false;
    this.readyPromise = undefined;
    this.readyResolve = undefined;
    this.readyReject = undefined;
    this.closePromise = undefined;
    this.context = undefined;
  }

  private async stopProcess(reason: Error): Promise<void> {
    const proc = this.process;
    if (!proc) {
      this.rejectPending(reason);
      this.readyReject?.(reason);
      this.ready = false;
      this.readyPromise = undefined;
      this.readyResolve = undefined;
      this.readyReject = undefined;
      return;
    }
    try {
      proc.stdin.end();
    } catch {
    }
    this.readyReject?.(reason);
    this.rejectPending(reason);
    const closed = this.closePromise ?? trackProcess(proc, projectProcessOwner(this.ownerRoot()));
    let closedGracefully = false;
    await Promise.race([
      closed.then(() => {
        closedGracefully = true;
      }),
      delay(500),
    ]);
    if (!closedGracefully) {
      await terminateProcess(proc);
    }
    this.clearProcess(proc);
  }
}
