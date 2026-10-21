// Renium bug-report ingest: validates a report bundle before it reaches R2
// and answers with the ID under which it was stored.
//
// POST /v1/reports            body: the report zip (application/zip)
//                             headers: X-Renium-Report-Id, X-Renium-Version
//   -> 201 { ok, id, key, bytes, sha256, receivedAt }
// GET  /health                -> { ok: true }
//
// Stored bundles are read with wrangler (npm run fetch -- RNM-...); the
// Worker exposes no download route.
//
// Only the archive shape `rbx report` writes is accepted: plain deflated or
// stored entries, no extra fields, descriptors, comments, zip64 or
// encryption, local headers equal to their directory records, and entries
// laid out back to back with nothing between them. Every entry is inflated
// under hard caps and its CRC checked, so a record that lies about sizes or
// hides a bomb never reaches the bucket. A per-address limiter, a global
// limiter and a daily budget (the Quota Durable Object) bound how much
// anyone can store; a missing binding fails closed.

const REPORT_ID = /^RNM-(\d{4})(\d{2})(\d{2})-[A-Z0-9]{6}$/;
const VERSION = /^\d{1,4}\.\d{1,4}\.\d{1,4}(-[0-9A-Za-z.]{1,24})?$/;
const GENERATED = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})Z$/;
const RESERVED_BASENAMES = /^(con|prn|aux|nul|com[1-9]|lpt[1-9])(\..*)?$/i;
const REGULAR_FILE_TYPES = new Set([0, 0o100000]);
const MAX_ENTRIES = 64;
const MAX_ENTRY_BYTES = 8 * 1024 * 1024;
const MAX_UNPACKED_BYTES = 24 * 1024 * 1024;
const MAX_REPORT_JSON_BYTES = 512 * 1024;
const MAX_JSON_BYTES = 256 * 1024;
const MAX_JSON_DEPTH = 32;
const MAX_MESSAGE_CHARS = 2000;
const EOCD_SEARCH_BYTES = 22 + 65535;
const ZIP64_MARKER = 0xffffffff;
const LANGUAGE_ENCODING_FLAG = 0x0800;
const REQUIRED_FILES = ["report.md", "report.json"];
const ALLOWED_FILES = new Set([
  ...REQUIRED_FILES,
  "doctor.json",
  "studio.json",
  "daemon.log",
  "transcript.md",
  "renium.project.jsonc",
  "renium.project.json",
]);
const CRASH_FILE = /^crashes\/[A-Za-z0-9._-]{1,120}\.json$/;
const ENTRY_NAME = /^[A-Za-z0-9._\/-]{1,128}$/;

export { objectKey, indexKey, readCentralDirectory, validateEntries, crc32 };

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    try {
      if (url.pathname === "/health") {
        return json({ ok: true }, 200);
      }
      if (url.pathname === "/v1/reports" && request.method === "POST") {
        return await ingest(request, env);
      }
      return json({ ok: false, error: "Not found" }, 404);
    } catch (error) {
      if (error instanceof RequestError) {
        return json({ ok: false, error: error.message }, error.status, error.headers);
      }
      console.error("renium-reports failure", error);
      return json({ ok: false, error: "Internal error" }, 500);
    }
  },
};

// One Durable Object instance holds the day's upload count and byte total.
// Reservations are made after validation and before the bucket writes.
export class Quota {
  constructor(state, env) {
    this.state = state;
    this.env = env;
  }

  async fetch(request) {
    const { bytes } = await request.json();
    const day = new Date().toISOString().slice(0, 10);
    const stored = (await this.state.storage.get("day")) || {};
    const record = stored.day === day ? stored : { day, uploads: 0, bytes: 0 };
    const maxUploads = Number(this.env.DAILY_UPLOADS) || 200;
    const maxBytes = Number(this.env.DAILY_BYTES) || 1024 * 1024 * 1024;
    if (record.uploads + 1 > maxUploads || record.bytes + bytes > maxBytes) {
      return json({ ok: false, day, uploads: record.uploads, bytes: record.bytes }, 429);
    }
    record.uploads += 1;
    record.bytes += bytes;
    await this.state.storage.put("day", record);
    return json({ ok: true, day, uploads: record.uploads, bytes: record.bytes }, 200);
  }
}

class RequestError extends Error {
  constructor(status, message, headers) {
    super(message);
    this.status = status;
    this.headers = headers;
  }
}

function json(body, status, headers) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json; charset=utf-8", "cache-control": "no-store", ...headers },
  });
}

async function ingest(request, env) {
  for (const binding of ["REPORTS", "UPLOADS", "GLOBAL", "QUOTA"]) {
    if (!env[binding]) {
      console.error(`renium-reports binding ${binding} is missing`);
      throw new RequestError(503, "The report store is not configured; keep the zip and attach it to a GitHub issue", {
        "retry-after": "3600",
      });
    }
  }
  const limit = Number(env.MAX_UPLOAD_BYTES) || 8 * 1024 * 1024;
  const ip = request.headers.get("cf-connecting-ip");
  if (!ip) {
    throw new RequestError(400, "Upload has no client address");
  }
  if (!(await env.UPLOADS.limit({ key: ip })).success) {
    throw new RequestError(429, "Too many uploads from this address; try again in a minute", { "retry-after": "60" });
  }
  if (!(await env.GLOBAL.limit({ key: "all" })).success) {
    throw new RequestError(429, "The report store is busy; try again in a minute", { "retry-after": "60" });
  }
  const type = (request.headers.get("content-type") || "").split(";")[0].trim().toLowerCase();
  if (type !== "application/zip" && type !== "application/octet-stream") {
    throw new RequestError(415, "Send the report zip as application/zip");
  }
  const declared = Number(request.headers.get("content-length"));
  if (Number.isFinite(declared) && declared > limit) {
    throw new RequestError(413, `Report exceeds ${limit} bytes`);
  }
  const bytes = await readLimited(request.body, limit);
  const entries = readCentralDirectory(bytes);
  const report = await validateEntries(bytes, entries);
  const claimed = request.headers.get("x-renium-report-id");
  if (claimed && claimed !== report.id) {
    throw new RequestError(400, "Report ID header does not match report.json");
  }
  const key = objectKey(report.id);
  if (await env.REPORTS.head(key)) {
    throw new RequestError(409, `Report ${report.id} was already uploaded`);
  }
  await reserveQuota(env.QUOTA, bytes.length);
  const receivedAt = new Date().toISOString();
  const digest = hex(await crypto.subtle.digest("SHA-256", bytes));
  const customMetadata = {
    reportId: report.id,
    version: report.version,
    os: report.os,
    arch: report.arch,
    generated: report.generated,
    receivedAt,
    sha256: digest,
    entries: String(entries.length),
    unpackedBytes: String(report.unpackedBytes),
    message: report.message.slice(0, 200),
    client: await hashedClient(ip, env.IP_SALT),
  };
  // The bundle is created exactly once and carries the metadata itself; the
  // index is a convenience written afterwards, so a losing concurrent upload
  // never touches it and a failed index write still leaves a complete report.
  const stored = await env.REPORTS.put(key, bytes, {
    httpMetadata: { contentType: "application/zip" },
    customMetadata,
    onlyIf: { etagDoesNotMatch: "*" },
  });
  if (!stored) {
    throw new RequestError(409, `Report ${report.id} was already uploaded`);
  }
  try {
    await env.REPORTS.put(indexKey(report.id), JSON.stringify({ key, ...customMetadata, bytes: bytes.length }), {
      httpMetadata: { contentType: "application/json" },
    });
  } catch (error) {
    console.error(`renium-reports index write failed for ${report.id}`, error);
  }
  return json({ ok: true, id: report.id, key, bytes: bytes.length, sha256: digest, receivedAt }, 201);
}

async function reserveQuota(namespace, bytes) {
  const stub = namespace.get(namespace.idFromName("global"));
  const response = await stub.fetch("https://quota/reserve", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ bytes }),
  });
  if (response.status === 429) {
    throw new RequestError(
      503,
      "The report store has reached its daily budget; keep the zip and attach it to a GitHub issue, or try again tomorrow",
      { "retry-after": "3600" },
    );
  }
  if (!response.ok) {
    throw new Error(`quota reservation answered ${response.status}`);
  }
}

async function readLimited(body, limit) {
  if (!body) {
    throw new RequestError(400, "Empty upload");
  }
  const chunks = [];
  let total = 0;
  const reader = body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > limit) {
      await reader.cancel();
      throw new RequestError(413, `Report exceeds ${limit} bytes`);
    }
    chunks.push(value);
  }
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

// Zip central directory walk. Every end-record candidate whose comment
// length closes the file is tried, so a comment that happens to hold the
// signature cannot hide or replace the real directory.
function readCentralDirectory(bytes) {
  if (bytes.length < 22) {
    throw new RequestError(400, "Report is not a zip file");
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let firstError = null;
  for (let i = bytes.length - 22; i >= Math.max(0, bytes.length - EOCD_SEARCH_BYTES); i--) {
    if (view.getUint32(i, true) !== 0x06054b50 || i + 22 + view.getUint16(i + 20, true) !== bytes.length) {
      continue;
    }
    try {
      return parseDirectory(bytes, view, i);
    } catch (error) {
      if (!(error instanceof RequestError)) throw error;
      firstError = firstError || error;
    }
  }
  throw firstError || new RequestError(400, "Report is not a zip file");
}

function parseDirectory(bytes, view, eocd) {
  const disk = view.getUint16(eocd + 4, true);
  const directoryDisk = view.getUint16(eocd + 6, true);
  const countHere = view.getUint16(eocd + 8, true);
  const count = view.getUint16(eocd + 10, true);
  const directorySize = view.getUint32(eocd + 12, true);
  const directoryOffset = view.getUint32(eocd + 16, true);
  if (disk !== 0 || directoryDisk !== 0 || countHere !== count) {
    throw new RequestError(400, "Report zip spans several disks");
  }
  if (count === 0) {
    throw new RequestError(400, "Report zip is empty");
  }
  if (count > MAX_ENTRIES) {
    throw new RequestError(400, `Report zip holds ${count} entries; at most ${MAX_ENTRIES} are allowed`);
  }
  if (directoryOffset + directorySize !== eocd) {
    throw new RequestError(400, "Report zip directory is damaged");
  }
  const entries = [];
  let cursor = directoryOffset;
  const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
  for (let i = 0; i < count; i++) {
    if (cursor + 46 > eocd || view.getUint32(cursor, true) !== 0x02014b50) {
      throw new RequestError(400, "Report zip directory is damaged");
    }
    const flags = view.getUint16(cursor + 8, true);
    const method = view.getUint16(cursor + 10, true);
    const crc = view.getUint32(cursor + 16, true);
    const compressedSize = view.getUint32(cursor + 20, true);
    const size = view.getUint32(cursor + 24, true);
    const nameLength = view.getUint16(cursor + 28, true);
    const extraLength = view.getUint16(cursor + 30, true);
    const commentLength = view.getUint16(cursor + 32, true);
    const startDisk = view.getUint16(cursor + 34, true);
    const externalAttributes = view.getUint32(cursor + 38, true);
    const localOffset = view.getUint32(cursor + 42, true);
    if (!REGULAR_FILE_TYPES.has((externalAttributes >>> 16) & 0o170000) || (externalAttributes & 0x10) !== 0) {
      throw new RequestError(400, "Report zip entry is not a regular file");
    }
    if (compressedSize === ZIP64_MARKER || size === ZIP64_MARKER || localOffset === ZIP64_MARKER) {
      throw new RequestError(400, "Report zip uses zip64 fields");
    }
    if ((flags & ~LANGUAGE_ENCODING_FLAG) !== 0) {
      throw new RequestError(400, "Report zip entries use encryption, data descriptors or other unsupported flags");
    }
    if (extraLength !== 0 || commentLength !== 0) {
      throw new RequestError(400, "Report zip entries carry extra fields or comments");
    }
    if (startDisk !== 0) {
      throw new RequestError(400, "Report zip spans several disks");
    }
    if (nameLength === 0) {
      throw new RequestError(400, "Report zip has an unnamed entry");
    }
    if (method !== 0 && method !== 8) {
      throw new RequestError(400, "Report zip entry uses an unsupported compression");
    }
    if (method === 0 && compressedSize !== size) {
      throw new RequestError(400, "Report zip entry size does not match its directory record");
    }
    const nameBytes = bytes.subarray(cursor + 46, cursor + 46 + nameLength);
    let name;
    try {
      name = decoder.decode(nameBytes);
    } catch {
      throw new RequestError(400, "Report zip has an entry name that is not UTF-8");
    }
    entries.push({ name, nameBytes, method, flags, crc, compressedSize, size, localOffset });
    cursor += 46 + nameLength;
  }
  if (cursor !== eocd) {
    throw new RequestError(400, "Report zip directory is damaged");
  }
  // Local headers must mirror their records and sit back to back from byte
  // zero to the directory, so nothing unlisted can ride along.
  const ordered = [...entries].sort((a, b) => a.localOffset - b.localOffset);
  let expectedOffset = 0;
  for (const entry of ordered) {
    const local = entry.localOffset;
    if (local !== expectedOffset || local + 30 > directoryOffset) {
      throw new RequestError(400, "Report zip holds data outside its listed entries");
    }
    if (view.getUint32(local, true) !== 0x04034b50) {
      throw new RequestError(400, "Report zip entry header is damaged");
    }
    const localFlags = view.getUint16(local + 6, true);
    const localMethod = view.getUint16(local + 8, true);
    const localCrc = view.getUint32(local + 14, true);
    const localCompressedSize = view.getUint32(local + 18, true);
    const localSize = view.getUint32(local + 22, true);
    const localNameLength = view.getUint16(local + 26, true);
    const localExtraLength = view.getUint16(local + 28, true);
    if (
      localFlags !== entry.flags ||
      localMethod !== entry.method ||
      localCrc !== entry.crc ||
      localCompressedSize !== entry.compressedSize ||
      localSize !== entry.size ||
      localExtraLength !== 0 ||
      localNameLength !== entry.nameBytes.length ||
      !sameBytes(bytes.subarray(local + 30, local + 30 + localNameLength), entry.nameBytes)
    ) {
      throw new RequestError(400, "Report zip entry header disagrees with its directory record");
    }
    entry.dataStart = local + 30 + localNameLength;
    entry.dataEnd = entry.dataStart + entry.compressedSize;
    if (entry.dataEnd > directoryOffset) {
      throw new RequestError(400, "Report zip entry data is truncated");
    }
    expectedOffset = entry.dataEnd;
  }
  if (expectedOffset !== directoryOffset) {
    throw new RequestError(400, "Report zip holds data outside its listed entries");
  }
  return entries;
}

function sameBytes(left, right) {
  if (left.length !== right.length) return false;
  for (let i = 0; i < left.length; i++) {
    if (left[i] !== right[i]) return false;
  }
  return true;
}

async function validateEntries(bytes, entries) {
  let unpacked = 0;
  const seen = new Set();
  const seenFolded = new Set();
  let reportJson = null;
  for (const entry of entries) {
    if (!ENTRY_NAME.test(entry.name) || entry.name.includes("..") || entry.name.startsWith("/")) {
      throw new RequestError(400, "Report zip entry has an unsafe name");
    }
    if (!ALLOWED_FILES.has(entry.name) && !CRASH_FILE.test(entry.name)) {
      throw new RequestError(400, `Report zip entry ${entry.name} is not part of a Renium report`);
    }
    if (RESERVED_BASENAMES.test(entry.name.slice(entry.name.lastIndexOf("/") + 1))) {
      throw new RequestError(400, `Report zip entry ${entry.name} uses a reserved file name`);
    }
    // Names are compared case-insensitively: the bundle is opened on
    // Windows and macOS, where two such names collide.
    const folded = entry.name.toLowerCase();
    if (seenFolded.has(folded)) {
      throw new RequestError(400, `Report zip lists ${entry.name} twice`);
    }
    seen.add(entry.name);
    seenFolded.add(folded);
    if (entry.size > MAX_ENTRY_BYTES || entry.compressedSize > MAX_ENTRY_BYTES) {
      throw new RequestError(400, `Report zip entry ${entry.name} is larger than ${MAX_ENTRY_BYTES} bytes`);
    }
    unpacked += entry.size;
    if (unpacked > MAX_UNPACKED_BYTES) {
      throw new RequestError(400, "Report zip unpacks to more than the allowed size");
    }
    if (entry.name.endsWith(".json") && entry.size > (entry.name === "report.json" ? MAX_REPORT_JSON_BYTES : MAX_JSON_BYTES)) {
      throw new RequestError(400, `Report zip entry ${entry.name} is larger than allowed`);
    }
    const data = await readEntryBytes(bytes, entry);
    if (crc32(data) !== entry.crc) {
      throw new RequestError(400, `Report zip entry ${entry.name} fails its checksum`);
    }
    checkEntryContent(entry.name, data);
    if (entry.name === "report.json") {
      if (data.length > MAX_REPORT_JSON_BYTES) {
        throw new RequestError(400, "report.json is larger than allowed");
      }
      reportJson = data;
    }
  }
  for (const name of REQUIRED_FILES) {
    if (!seen.has(name)) {
      throw new RequestError(400, `Report zip has no ${name}`);
    }
  }
  let parsed;
  try {
    parsed = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(reportJson));
  } catch {
    throw new RequestError(400, "report.json is not valid JSON");
  }
  if (!isObject(parsed) || !isObject(parsed.environment)) {
    throw new RequestError(400, "report.json has no environment block");
  }
  const environment = parsed.environment;
  const id = typeof environment.id === "string" ? environment.id : "";
  const idMatch = REPORT_ID.exec(id);
  if (!idMatch || !isCalendarDate(idMatch[1], idMatch[2], idMatch[3])) {
    throw new RequestError(400, "report.json carries no valid report ID");
  }
  const version = typeof environment.version === "string" ? environment.version : "";
  if (!VERSION.test(version)) {
    throw new RequestError(400, "report.json carries no valid Renium version");
  }
  const generated = typeof environment.generated === "string" ? environment.generated : "";
  const generatedMatch = GENERATED.exec(generated);
  if (!generatedMatch || !isCalendarTime(generatedMatch.slice(1))) {
    throw new RequestError(400, "report.json carries no valid generation time");
  }
  const os = boundedString(environment.os, 16);
  const arch = boundedString(environment.arch, 16);
  if (os === null || arch === null) {
    throw new RequestError(400, "report.json carries no valid platform");
  }
  const expected = [...seen].filter((name) => name !== "report.json").sort();
  const listed = Array.isArray(parsed.files) ? parsed.files : null;
  if (!listed || listed.some((name) => typeof name !== "string") || new Set(listed).size !== listed.length) {
    throw new RequestError(400, "report.json has no valid files list");
  }
  const sortedListed = [...listed].sort();
  if (sortedListed.length !== expected.length || sortedListed.some((name, index) => name !== expected[index])) {
    throw new RequestError(400, "report.json lists files other than the zip's entries");
  }
  if (parsed.message !== undefined && parsed.message !== null && typeof parsed.message !== "string") {
    throw new RequestError(400, "report.json carries an invalid message");
  }
  const message = typeof parsed.message === "string" ? parsed.message.slice(0, MAX_MESSAGE_CHARS) : "";
  return { id, version, os, arch, generated, message, unpackedBytes: unpacked };
}

function isObject(value) {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function boundedString(value, limit) {
  return typeof value === "string" && value.length >= 1 && value.length <= limit && /^[A-Za-z0-9_.-]+$/.test(value) ? value : null;
}

function isCalendarTime([year, month, day, hour, minute, second]) {
  return isCalendarDate(year, month, day) && Number(hour) < 24 && Number(minute) < 60 && Number(second) < 60;
}

function isCalendarDate(year, month, day) {
  const date = new Date(Date.UTC(Number(year), Number(month) - 1, Number(day)));
  return (
    date.getUTCFullYear() === Number(year) &&
    date.getUTCMonth() === Number(month) - 1 &&
    date.getUTCDate() === Number(day) &&
    Number(year) >= 2025 &&
    Number(year) <= 2100
  );
}

// Every report file is text: UTF-8 throughout, and the .json files parse.
// A binary smuggled under a report file name is refused here.
function checkEntryContent(name, data) {
  let text;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(data);
  } catch {
    throw new RequestError(400, `Report zip entry ${name} is not UTF-8 text`);
  }
  if (text.includes("\0")) {
    throw new RequestError(400, `Report zip entry ${name} holds binary data`);
  }
  if (name.endsWith(".json")) {
    if (jsonDepth(text) > MAX_JSON_DEPTH) {
      throw new RequestError(400, `Report zip entry ${name} nests deeper than allowed`);
    }
    if (name !== "report.json") {
      try {
        JSON.parse(text);
      } catch {
        throw new RequestError(400, `Report zip entry ${name} is not valid JSON`);
      }
    }
  }
}

// Deepest bracket nesting outside string literals: parsing a tiny file of
// millions of nested arrays would otherwise allocate hundreds of megabytes.
function jsonDepth(text) {
  let depth = 0;
  let deepest = 0;
  let inString = false;
  let escaped = false;
  for (let i = 0; i < text.length; i++) {
    const char = text[i];
    if (inString) {
      if (escaped) {
        escaped = false;
      } else if (char === "\\") {
        escaped = true;
      } else if (char === '"') {
        inString = false;
      }
      continue;
    }
    if (char === '"') {
      inString = true;
    } else if (char === "[" || char === "{") {
      depth++;
      if (depth > deepest) {
        deepest = depth;
        if (deepest > MAX_JSON_DEPTH) {
          return deepest;
        }
      }
    } else if (char === "]" || char === "}") {
      depth--;
    }
  }
  return deepest;
}

// Inflates one entry under its declared size so a record that lies about
// its size, or hides a highly compressible payload, is refused here rather
// than discovered by whoever opens the bundle. The stream must also use
// every compressed byte: a deflate stream always ends inside its last byte,
// so if the data minus that byte still inflates to the same output, the
// tail was not part of the stream and could carry anything.
async function readEntryBytes(bytes, entry) {
  const data = bytes.subarray(entry.dataStart, entry.dataEnd);
  if (entry.method === 0) {
    return data;
  }
  let inflated;
  try {
    inflated = await inflateRaw(data, entry.size + 1);
  } catch (error) {
    if (error instanceof RequestError && error.status === 413) {
      throw new RequestError(400, `Report zip entry ${entry.name} inflates past its directory record`);
    }
    throw new RequestError(400, `Report zip entry ${entry.name} is not valid deflate data`);
  }
  if (inflated.length !== entry.size) {
    throw new RequestError(400, `Report zip entry ${entry.name} size does not match its directory record`);
  }
  let shortened = null;
  try {
    shortened = await inflateRaw(data.subarray(0, data.length - 1), entry.size + 1);
  } catch {
    shortened = null;
  }
  if (shortened && sameBytes(shortened, inflated)) {
    throw new RequestError(400, `Report zip entry ${entry.name} carries bytes after its deflate stream`);
  }
  return inflated;
}

async function inflateRaw(data, limit) {
  const stream = new Blob([data]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
  return readLimited(stream, limit);
}

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c >>> 0;
  }
  return table;
})();

function crc32(data) {
  let crc = 0xffffffff;
  for (let i = 0; i < data.length; i++) {
    crc = CRC_TABLE[(crc ^ data[i]) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function objectKey(id) {
  const date = id.slice(4, 12);
  return `reports/${date.slice(0, 4)}/${date.slice(4, 6)}/${date.slice(6, 8)}/${id}.zip`;
}

function indexKey(id) {
  return `index/${id}.json`;
}

function hex(buffer) {
  return [...new Uint8Array(buffer)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function hashedClient(ip, salt) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(`${salt || ""}|${ip}`));
  return hex(digest).slice(0, 16);
}
