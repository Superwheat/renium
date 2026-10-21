// Downloads a stored bug report by ID: npm run fetch -- RNM-20261021-AB12CD [DIR]
import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";

import { indexKey, objectKey } from "../src/index.mjs";

const id = process.argv[2];
if (!id || !/^RNM-\d{8}-[A-Z0-9]{6}$/.test(id)) {
  console.error("Usage: npm run fetch -- RNM-YYYYMMDD-XXXXXX [DIR]");
  process.exit(2);
}
const directory = path.resolve(process.argv[3] || path.join("downloads", id));
mkdirSync(directory, { recursive: true });

let wranglerEntry;
try {
  wranglerEntry = createRequire(import.meta.url).resolve("wrangler/bin/wrangler.js");
} catch {
  console.error("wrangler is not installed here; run npm install first");
  process.exit(2);
}

// The bundle is authoritative; the index is a convenience that may be absent.
function wrangler(key, file, required) {
  const result = spawnSync(
    process.execPath,
    [wranglerEntry, "r2", "object", "get", `renium-reports/${key}`, "--file", file, "--remote"],
    { stdio: "inherit", shell: false },
  );
  if (result.status !== 0) {
    if (required) {
      process.exit(result.status ?? 1);
    }
    console.error(`No ${key}; the bundle's own metadata carries the same fields`);
  }
}

wrangler(objectKey(id), path.join(directory, `${id}.zip`), true);
wrangler(indexKey(id), path.join(directory, "index.json"), false);
console.log(`Report ${id} saved to ${directory}`);
