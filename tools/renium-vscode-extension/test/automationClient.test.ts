import assert from "node:assert/strict";
import Module from "node:module";
import { test } from "node:test";

const loader = Module as unknown as { _load(request: string, parent: NodeModule | null, isMain: boolean): unknown };
const original = loader._load;
loader._load = (request, parent, isMain) => request === "vscode" ? {} : original.call(loader, request, parent, isMain);
const { AutomationClient } = require("../src/automationClient") as typeof import("../src/automationClient");
loader._load = original;

test("global audio bypasses project binding while selected-window audio retains it", async () => {
  const { AUTOMATION_OP } = require("../src/automationProtocol.generated");
  for (const global of [true, false]) {
    const client = new AutomationClient({ appendLine() {} } as never, () => "") as any;
    let binds = 0;
    client.ensure = async () => {};
    client.ensureContext = async () => { binds++; return 42; };
    client.send = async (_config: unknown, _label: string, op: number, cx: number | undefined) => {
      assert.equal(op, AUTOMATION_OP.studioAudio);
      assert.equal(cx, global ? undefined : 42);
      return { code: 0 };
    };
    await client.runOperation("rbx", {}, "audio", AUTOMATION_OP.studioAudio, { global, action: "auto" });
    assert.equal(binds, global ? 0 : 1);
  }
});

test("unmatched connected Studios cannot cause a bind loop; a newly ready target can bind", async () => {
  for (const ready of [false, true]) {
    const client = new AutomationClient({ appendLine() {} } as never, () => "") as any;
    let binds = 0;
    let statuses = 0;
    client.send = async (_config: unknown, label: string) => {
      if (label === "bind") {
        binds++;
        assert.ok(binds <= 2, "must not spin on unrelated windows");
        return { code: 0, result: { id: 1, runtimeId: ready && binds === 2 ? "target" : null } };
      }
      assert.equal(label, "studios");
      statuses++;
      return { code: 0, result: { clients: [{ runtimeId: "connected" }] } };
    };
    const result = client.ensureContext({ projectRoot: process.cwd(), bridgeWaitSeconds: 1 }, true);
    if (ready) { assert.equal(await result, 1); }
    else { await assert.rejects(result, /do not match this project's target/); }
    assert.equal(binds, 2);
    assert.equal(statuses, 1);
  }
});

function reader(): { feed(data: Buffer | string): void; lines: string[]; errors: Error[] } {
  const lines: string[] = [];
  const errors: Error[] = [];
  const client = new AutomationClient({ appendLine() {} } as never, () => "") as unknown as {
    handleOutput(prefix: string, data: Buffer | string, stderr: boolean): void;
    processLine(line: string): void;
    stop(error: Error): Promise<void>;
  };
  client.processLine = line => { lines.push(line); };
  client.stop = async error => { errors.push(error); };
  return { feed: data => client.handleOutput("test", data, false), lines, errors };
}

test("protocol framing accepts large replies and joined frames without an aggregate limit", () => {
  const frame = JSON.stringify({ value: "x".repeat(1_100_000) });
  const joined = `${frame}\n`.repeat(8);
  for (const size of [65_536, joined.length]) {
    const r = reader();
    for (let offset = 0; offset < joined.length; offset += size) {
      r.feed(joined.slice(offset, offset + size));
    }
    assert.deepEqual(r.errors, []);
    assert.deepEqual(r.lines, Array(8).fill(frame));
  }
});

test("protocol framing preserves UTF-8 split anywhere, CRLF, and trailing partial lines", () => {
  const frame = JSON.stringify({ text: "é🚗こんにちは" });
  const bytes = Buffer.from(`${frame}\r\npartial`);
  const r = reader();
  for (const byte of bytes) { r.feed(Buffer.from([byte])); }
  assert.deepEqual(r.lines, [frame]);
  r.feed(" end\n");
  assert.deepEqual(r.lines, [frame, "partial end"]);
  assert.deepEqual(r.errors, []);
});

test("protocol size bound counts bytes per frame and accepts the exact boundary", () => {
  const limit = 8 * 1024 * 1024;
  const exact = reader();
  exact.feed(Buffer.alloc(limit, 120));
  exact.feed("\n");
  assert.equal(exact.lines[0].length, limit);
  assert.deepEqual(exact.errors, []);
  for (const suffix of ["x", "x\n"]) {
    const oversized = reader();
    oversized.feed(Buffer.from("é".repeat(limit / 2)));
    oversized.feed(suffix);
    assert.equal(oversized.errors.length, 1);
    assert.equal(oversized.lines.length, 0);
  }
});

test("one request timeout cancels only its ID with a capable proxy", async () => {
  const writes: string[] = [];
  const stops: Error[] = [];
  const client = new AutomationClient({ appendLine() {} } as never, () => "") as any;
  client.process = { killed: false, stdin: {
    writable: true,
    write: (text: string, _encoding: string, callback: (error?: Error) => void) => {
      writes.push(text);
      callback();
    },
  } };
  client.stop = async (error: Error) => { stops.push(error); };
  client.requestTimeoutMs = (_config: unknown, _op: number, timeout: number) => timeout;
  const config = { progressHeartbeatSeconds: 2 };
  // Capability arrives in ordinary replies; no extra startup request is needed.
  client.processLine(JSON.stringify({ v: 1, id: 0, ok: 1, proxyCancellation: true }));
  const slow = client.send(config, "slow", 1, undefined, {}, { quietWait: true, timeoutMs: 5 });
  const healthy = client.send(config, "healthy", 1, undefined, {}, { quietWait: true, timeoutMs: 1_000 });
  const result = await slow;
  assert.equal(result.code, 124);
  assert.deepEqual(JSON.parse(writes[2]), { cancel: 1 });
  assert.equal(client.pending.size, 1);
  assert.deepEqual(stops, []);
  client.processLine(JSON.stringify({ v: 1, id: 1, ok: 1, r: "late", proxyCancellation: true }));
  assert.equal(client.pending.size, 1, "late reply must not finish another request");
  client.processLine(JSON.stringify({ v: 1, id: 2, ok: 1, r: "healthy", proxyCancellation: true }));
  assert.equal((await healthy).result, "healthy");
  client.clearProcess(client.process);
  assert.equal(client.proxyCancellation, false);
});
