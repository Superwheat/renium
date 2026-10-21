import test from "node:test";
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { deflateRawSync } from "node:zlib";

import worker, { Quota, crc32, objectKey } from "../src/index.mjs";

const ID = "RNM-20261021-AB12CD";
const CLIENT = { "cf-connecting-ip": "203.0.113.9" };

// Builds a zip from [name, content, options] triples; options override the
// records so tests can lie the way a hostile client would.
function zip(entries, { stored = false, badSize = false, trailer = null, comment = "", gap = null } = {}) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  const offsets = [];
  for (const [name, content, options = {}] of entries) {
    const nameBytes = Buffer.from(name, "utf-8");
    const data = Buffer.isBuffer(content) ? content : Buffer.from(content, "utf-8");
    const packed = options.packed ?? (stored ? data : deflateRawSync(data));
    const method = stored ? 0 : 8;
    const size = options.declaredSize ?? (badSize ? data.length + 7 : data.length);
    const flags = options.flags ?? 0;
    const crc = options.crc ?? crc32(data);
    const extra = options.extra ?? Buffer.alloc(0);
    const localExtra = options.localExtra ?? extra;
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(options.localFlags ?? flags, 6);
    local.writeUInt16LE(options.localMethod ?? method, 8);
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(packed.length, 18);
    local.writeUInt32LE(size, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    local.writeUInt16LE(localExtra.length, 28);
    locals.push(local, nameBytes, localExtra, packed);
    offsets.push(offset);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(flags, 8);
    central.writeUInt16LE(method, 10);
    central.writeUInt32LE(crc, 16);
    central.writeUInt32LE(packed.length, 20);
    central.writeUInt32LE(size, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt16LE(extra.length, 30);
    central.writeUInt32LE(options.externalAttributes ?? 0x81a40000, 38);
    central.writeUInt32LE(offset, 42);
    centrals.push(central, nameBytes, extra);
    offset += local.length + nameBytes.length + localExtra.length + packed.length;
    if (gap && options.gapAfter) {
      locals.push(gap);
      offset += gap.length;
    }
  }
  for (const [index, [, , options = {}]] of entries.entries()) {
    if (options.localOffsetOf !== undefined) {
      centrals[index * 3].writeUInt32LE(offsets[options.localOffsetOf], 42);
    }
  }
  const directory = Buffer.concat(centrals);
  const commentBytes = Buffer.from(comment, "latin1");
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(entries.length, 8);
  eocd.writeUInt16LE(entries.length, 10);
  eocd.writeUInt32LE(directory.length, 12);
  eocd.writeUInt32LE(offset, 16);
  eocd.writeUInt16LE(commentBytes.length, 20);
  return Buffer.concat([...locals, directory, eocd, commentBytes, ...(trailer ? [trailer] : [])]);
}

// An unlisted local record: a complete entry written between listed ones
// that no central record names.
function unlistedLocal(name, content) {
  const nameBytes = Buffer.from(name);
  const data = Buffer.from(content);
  const packed = deflateRawSync(data);
  const local = Buffer.alloc(30);
  local.writeUInt32LE(0x04034b50, 0);
  local.writeUInt16LE(20, 4);
  local.writeUInt16LE(8, 8);
  local.writeUInt32LE(crc32(data), 14);
  local.writeUInt32LE(packed.length, 18);
  local.writeUInt32LE(data.length, 22);
  local.writeUInt16LE(nameBytes.length, 26);
  return Buffer.concat([local, nameBytes, packed]);
}

function reportJson({ environment = {}, files, message = "push failed" } = {}) {
  return JSON.stringify({
    environment: {
      id: ID,
      generated: "2026-10-21T03:00:00Z",
      version: "0.4.9",
      os: "windows",
      arch: "x86_64",
      ...environment,
    },
    message,
    files: files ?? ["report.md", "doctor.json"],
  });
}

function bundle(extra = {}) {
  const others = [["report.md", "# Report\n"], ["doctor.json", "{}\n"], ...(extra.entries || [])];
  const files = extra.files ?? others.map(([name]) => name);
  return zip([...others, ["report.json", reportJson({ environment: extra.environment, files }), extra.reportOptions || {}]], extra.zipOptions);
}

function fakeQuota({ exhausted = false } = {}) {
  const calls = [];
  return {
    calls,
    namespace: {
      idFromName: (name) => ({ name }),
      get: () => ({
        fetch: async (url, init) => {
          calls.push(JSON.parse(init.body));
          return new Response(JSON.stringify({ ok: !exhausted }), { status: exhausted ? 429 : 200 });
        },
      }),
    },
  };
}

function fakeEnv({ limited = false, globalLimited = false, exhausted = false, headMisses = false } = {}) {
  const objects = new Map();
  const quota = fakeQuota({ exhausted });
  return {
    objects,
    quota,
    env: {
      MAX_UPLOAD_BYTES: "4096",
      UPLOADS: { limit: async () => ({ success: !limited }) },
      GLOBAL: { limit: async () => ({ success: !globalLimited }) },
      QUOTA: quota.namespace,
      REPORTS: {
        head: async (key) => (!headMisses && objects.has(key) ? { key } : null),
        put: async (key, value, options) => {
          if (options?.onlyIf?.etagDoesNotMatch === "*" && objects.has(key)) {
            return null;
          }
          objects.set(key, { value, options });
          return { key };
        },
      },
    },
  };
}

async function post(env, body, headers = {}) {
  const request = new Request("https://reports.test/v1/reports", {
    method: "POST",
    body,
    headers: { "content-type": "application/zip", ...CLIENT, ...headers },
  });
  const response = await worker.fetch(request, env);
  return { status: response.status, body: await response.json(), headers: response.headers };
}

test("stores a valid bundle under its report ID and answers with that ID", async () => {
  const { env, objects, quota } = fakeEnv();
  const { status, body } = await post(env, bundle(), { "x-renium-report-id": ID });
  assert.equal(status, 201);
  assert.equal(body.ok, true);
  assert.equal(body.id, ID);
  assert.equal(body.key, "reports/2026/10/21/RNM-20261021-AB12CD.zip");
  assert.equal(objectKey(ID), body.key);
  const stored = objects.get(body.key);
  assert.ok(stored);
  assert.equal(stored.options.customMetadata.version, "0.4.9");
  assert.equal(stored.options.customMetadata.message, "push failed");
  assert.match(body.sha256, /^[0-9a-f]{64}$/);
  const index = JSON.parse(objects.get(`index/${ID}.json`).value);
  assert.equal(index.key, body.key);
  assert.equal(index.bytes, body.bytes);
  assert.deepEqual(quota.calls, [{ bytes: body.bytes }]);
});

test("accepts stored entries, crash files, the project file, and zip comments even with a stray signature", async () => {
  assert.equal((await post(fakeEnv().env, bundle({ zipOptions: { stored: true } }))).status, 201);
  assert.equal((await post(fakeEnv().env, bundle({ zipOptions: { comment: "made by rbx" } }))).status, 201);
  const stray = "PK\x05\x06" + "\0".repeat(18);
  assert.equal((await post(fakeEnv().env, bundle({ zipOptions: { comment: stray } }))).status, 201);
  const full = bundle({ entries: [["crashes/2026-10-21T03-00-00.json", "{}"], ["renium.project.jsonc", "// comment\n{}"], ["daemon.log", "line\n"]] });
  assert.equal((await post(fakeEnv().env, full)).status, 201);
});

test("refuses a second upload of the same ID, also when two uploads race past the head check", async () => {
  const { env } = fakeEnv();
  assert.equal((await post(env, bundle())).status, 201);
  const { status, body } = await post(env, bundle());
  assert.equal(status, 409);
  assert.match(body.error, /already uploaded/);
  const raced = fakeEnv({ headMisses: true });
  const first = await post(raced.env, bundle());
  assert.equal(first.status, 201);
  assert.equal((await post(raced.env, bundle({ entries: [["daemon.log", "other\n"]] }))).status, 409);
  assert.equal(raced.objects.size, 2);
  assert.equal(JSON.parse(raced.objects.get(`index/${ID}.json`).value).sha256, first.body.sha256);
  const indexFails = fakeEnv();
  const put = indexFails.env.REPORTS.put;
  indexFails.env.REPORTS.put = async (key, value, options) => {
    if (key.startsWith("index/")) throw new Error("index unavailable");
    return put(key, value, options);
  };
  assert.equal((await post(indexFails.env, bundle())).status, 201);
  assert.equal(indexFails.objects.size, 1);
});

test("refuses a header ID that differs from report.json", async () => {
  const { env } = fakeEnv();
  const { status } = await post(env, bundle(), { "x-renium-report-id": "RNM-20261021-ZZZZZZ" });
  assert.equal(status, 400);
});

test("refuses bundles that are not zips, wrong content types, missing addresses and oversized bodies", async () => {
  const { env } = fakeEnv();
  assert.equal((await post(env, Buffer.from("not a zip at all, but long enough to pass"))).status, 400);
  assert.equal((await post(env, bundle(), { "content-type": "text/plain" })).status, 415);
  const big = bundle({ entries: [["daemon.log", randomBytes(6000).toString("base64")]] });
  assert.ok(big.length > 4096);
  assert.equal((await post(env, big)).status, 413);
  assert.equal((await post(env, Buffer.alloc(0))).status, 400);
  const request = new Request("https://reports.test/v1/reports", { method: "POST", body: bundle(), headers: { "content-type": "application/zip" } });
  assert.equal((await worker.fetch(request, env)).status, 400);
});

test("refuses entries outside the report layout and bundles missing required files", async () => {
  const { env } = fakeEnv();
  for (const name of ["../escape.txt", "notes.txt", "crashes/../report.md", "crashes/evil.exe", "src/index.mjs", "REPORT.JSON", "crashes/"]) {
    const { status, body } = await post(env, bundle({ entries: [[name, "x"]] }));
    assert.equal(status, 400, name);
    assert.match(body.error, /unsafe name|not part of a Renium report/, name);
  }
  assert.match((await post(env, zip([["report.md", "# Report\n"]]))).body.error, /no report\.json/);
  assert.match((await post(env, zip([["report.json", reportJson({ files: [] })]]))).body.error, /no report\.md/);
});

test("refuses report.json that is malformed, mistyped, or lists other files than the zip holds", async () => {
  const { env } = fakeEnv();
  const withReport = (text) => zip([["report.md", "# Report\n"], ["report.json", text]]);
  assert.match((await post(env, withReport("{not json"))).body.error, /not valid JSON/);
  assert.match((await post(env, withReport("[1,2]"))).body.error, /no environment block/);
  assert.match((await post(env, bundle({ environment: { id: "bogus" } }))).body.error, /valid report ID/);
  assert.match((await post(env, bundle({ environment: { id: "RNM-20261399-AB12CD" } }))).body.error, /valid report ID/);
  assert.match((await post(env, bundle({ environment: { version: "dev" } }))).body.error, /valid Renium version/);
  assert.match((await post(env, bundle({ environment: { version: "0.4.9junk" } }))).body.error, /valid Renium version/);
  assert.match((await post(env, bundle({ environment: { generated: "yesterday" } }))).body.error, /valid generation time/);
  assert.match((await post(env, bundle({ environment: { generated: "2026-13-45T25:61:61Z" } }))).body.error, /valid generation time/);
  assert.match((await post(env, bundle({ environment: { version: "0.4." + "9".repeat(3000) } }))).body.error, /valid Renium version/);
  assert.match((await post(env, bundle({ environment: { os: {} } }))).body.error, /valid platform/);
  assert.match((await post(env, bundle({ files: ["report.md"] }))).body.error, /lists files other than/);
  assert.match((await post(env, bundle({ files: ["report.md", "doctor.json", "daemon.log"] }))).body.error, /lists files other than/);
  assert.match((await post(env, bundle({ files: ["report.md", "doctor.json", "doctor.json"] }))).body.error, /no valid files list/);
  assert.match((await post(env, withReport(reportJson({ files: {} })))).body.error, /no valid files list/);
});

test("refuses directory records that lie about sizes or checksums", async () => {
  const { env } = fakeEnv();
  assert.match((await post(env, bundle({ zipOptions: { badSize: true } }))).body.error, /size does not match/);
  const bomb = bundle({ entries: [["daemon.log", "a".repeat(3000), { declaredSize: 10 }]] });
  const { status, body } = await post(env, bomb);
  assert.equal(status, 400);
  assert.match(body.error, /inflates past its directory record/);
  const storedLie = zip([["report.md", "# Report\n"], ["report.json", reportJson(), { declaredSize: 5 }]], { stored: true });
  assert.match((await post(env, storedLie)).body.error, /size does not match/);
  const badCrc = bundle({ entries: [["daemon.log", "line\n", { crc: 0x12345678 }]] });
  assert.match((await post(env, badCrc)).body.error, /fails its checksum/);
});

test("refuses trailing bytes after a deflate stream, case-colliding names and byte-order marks in names", async () => {
  const { env } = fakeEnv();
  const data = Buffer.from("line\n");
  const packed = Buffer.concat([deflateRawSync(data), Buffer.from("smuggled bytes")]);
  const trailing = bundle({ entries: [["daemon.log", "line\n", { packed }]] });
  assert.match((await post(env, trailing)).body.error, /bytes after its deflate stream/);
  const twoStreams = bundle({ entries: [["daemon.log", "line\n", { packed: Buffer.concat([deflateRawSync(data), deflateRawSync(Buffer.from("more"))]) }]] });
  assert.match((await post(env, twoStreams)).body.error, /bytes after its deflate stream/);
  const collide = bundle({ entries: [["crashes/a.json", "{}"], ["crashes/A.json", "{}"]] });
  assert.match((await post(env, collide)).body.error, /twice/);
  assert.match((await post(env, bundle({ entries: [["crashes/NUL.json", "{}"]] }))).body.error, /reserved file name/);
  assert.match((await post(env, bundle({ entries: [["crashes/com1.crash.json", "{}"]] }))).body.error, /reserved file name/);
  const symlink = bundle({ entries: [["daemon.log", "../../etc/passwd", { externalAttributes: 0xa1ff0000 }]] });
  assert.match((await post(env, symlink)).body.error, /not a regular file/);
  const dosDirectory = bundle({ entries: [["daemon.log", "x", { externalAttributes: 0x00000010 }]] });
  assert.match((await post(env, dosDirectory)).body.error, /not a regular file/);
  assert.equal((await post(env, bundle({ entries: [["daemon.log", "x", { externalAttributes: 0 }]] }))).status, 201);
  const bom = bundle({ entries: [["\ufeffdaemon.log", "line\n"]], files: ["report.md", "doctor.json", "daemon.log"] });
  assert.match((await post(env, bom)).body.error, /unsafe name/);
  assert.equal((await post(fakeEnv().env, bundle({ entries: [["daemon.log", "a".repeat(2000)]] }))).status, 201);
});

test("refuses JSON that nests too deep or exceeds the per-file JSON size", async () => {
  const { env } = fakeEnv();
  const deep = "[".repeat(2000) + "0" + "]".repeat(2000);
  assert.match((await post(env, bundle({ entries: [["crashes/deep.json", deep]] }))).body.error, /nests deeper/);
  const deepReport = zip([["report.md", "# Report\n"], ["report.json", JSON.stringify({ environment: { id: ID }, deep: JSON.parse("[".repeat(40) + "]".repeat(40)) })]]);
  assert.match((await post(env, deepReport)).body.error, /nests deeper/);
  const quoted = bundle({ entries: [["studio.json", JSON.stringify({ text: "[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[" })]] });
  assert.equal((await post(env, quoted)).status, 201);
  const big = fakeEnv();
  big.env.MAX_UPLOAD_BYTES = String(8 * 1024 * 1024);
  const oversized = bundle({ entries: [["studio.json", JSON.stringify({ pad: "x".repeat(300 * 1024) })]] });
  assert.match((await post(big.env, oversized)).body.error, /larger than allowed/);
});

test("refuses binary or non-JSON content under report file names", async () => {
  const { env } = fakeEnv();
  const binary = bundle({ entries: [["daemon.log", Buffer.from([0x4d, 0x5a, 0x90, 0x00, 0xff, 0xfe])]] });
  assert.match((await post(env, binary)).body.error, /not UTF-8 text|binary data/);
  assert.match((await post(env, bundle({ entries: [["daemon.log", "text\0text"]] }))).body.error, /binary data/);
  assert.match((await post(env, bundle({ entries: [["studio.json", "{nope"]] }))).body.error, /studio\.json is not valid JSON/);
  assert.match((await post(env, bundle({ entries: [["crashes/a.json", "[1, 2"]] }))).body.error, /crashes\/a\.json is not valid JSON/);
});

test("refuses archives shaped unlike the CLI's: extra fields, descriptors, unlisted data, overlaps, zip64, encryption, trailing bytes", async () => {
  const { env } = fakeEnv();
  const unicodePath = Buffer.concat([Buffer.from([0x75, 0x70, 0x0c, 0x00, 0x01, 0, 0, 0, 0]), Buffer.from("../x.txt")]);
  const aliased = bundle({ entries: [["daemon.log", "line\n", { extra: unicodePath }]] });
  assert.match((await post(env, aliased)).body.error, /extra fields/);
  const localOnlyExtra = bundle({ entries: [["daemon.log", "line\n", { extra: Buffer.alloc(0), localExtra: Buffer.from([1, 2, 3, 4]) }]] });
  assert.match((await post(env, localOnlyExtra)).body.error, /disagrees|outside its listed entries/);
  const descriptor = bundle({ entries: [["daemon.log", "line\n", { flags: 0x0008 }]] });
  assert.match((await post(env, descriptor)).body.error, /unsupported flags/);
  const smuggled = bundle({ entries: [["daemon.log", "line\n", { gapAfter: true }]], zipOptions: { gap: unlistedLocal("secrets.env", "TOKEN=abc") } });
  assert.match((await post(env, smuggled)).body.error, /outside its listed entries/);
  const overlap = zip([["report.md", "# Report\n"], ["report.json", reportJson({ files: ["report.md"] }), { localOffsetOf: 0 }]]);
  assert.match((await post(env, overlap)).body.error, /outside its listed entries|disagrees/);
  const zip64 = bundle({ reportOptions: { declaredSize: 0xffffffff } });
  assert.match((await post(env, zip64)).body.error, /zip64/);
  const encrypted = bundle({ reportOptions: { flags: 0x0001 } });
  assert.match((await post(env, encrypted)).body.error, /unsupported flags/);
  const localMethod = bundle({ reportOptions: { localMethod: 99 } });
  assert.match((await post(env, localMethod)).body.error, /disagrees/);
  const trailing = zip([["report.md", "# Report\n"], ["report.json", reportJson({ files: ["report.md"] })]], { trailer: Buffer.from("garbage after the end record") });
  assert.match((await post(env, trailing)).body.error, /not a zip file/);
  assert.equal((await post(env, bundle().subarray(0, -5))).status, 400);
});

test("rate limits by address and globally, stops at the daily budget, and fails closed without bindings", async () => {
  const perAddress = await post(fakeEnv({ limited: true }).env, bundle());
  assert.equal(perAddress.status, 429);
  assert.equal(perAddress.headers.get("retry-after"), "60");
  assert.equal((await post(fakeEnv({ globalLimited: true }).env, bundle())).status, 429);
  const { env, objects } = fakeEnv({ exhausted: true });
  const { status, body } = await post(env, bundle());
  assert.equal(status, 503);
  assert.match(body.error, /daily budget/);
  assert.equal(objects.size, 0);
  for (const binding of ["UPLOADS", "GLOBAL", "QUOTA", "REPORTS"]) {
    const bare = fakeEnv();
    delete bare.env[binding];
    const refused = await post(bare.env, bundle());
    assert.equal(refused.status, 503, binding);
    assert.equal(bare.objects.size, 0, binding);
  }
});

test("the quota object counts uploads and bytes per UTC day", async () => {
  const storage = new Map();
  const state = { storage: { get: async (key) => storage.get(key), put: async (key, value) => storage.set(key, value) } };
  const quota = new Quota(state, { DAILY_UPLOADS: "2", DAILY_BYTES: "1000" });
  const reserve = (bytes) => quota.fetch(new Request("https://quota/reserve", { method: "POST", body: JSON.stringify({ bytes }) }));
  assert.equal((await reserve(600)).status, 200);
  assert.equal((await reserve(600)).status, 429);
  assert.equal((await reserve(300)).status, 200);
  assert.equal((await reserve(1)).status, 429);
  const record = storage.get("day");
  assert.equal(record.uploads, 2);
  assert.equal(record.bytes, 900);
  storage.set("day", { day: "2000-01-01", uploads: 2, bytes: 900 });
  assert.equal((await reserve(1)).status, 200);
});

test("answers health and 404 for other routes", async () => {
  const { env } = fakeEnv();
  const health = await worker.fetch(new Request("https://reports.test/health"), env);
  assert.equal(health.status, 200);
  const other = await worker.fetch(new Request("https://reports.test/v1/reports/" + ID), env);
  assert.equal(other.status, 404);
});
