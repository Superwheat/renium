import assert from "node:assert/strict";
import test from "node:test";

import { withPausedFileWrites, type FileWriteControlRequest } from "../src/pausedFileWrites";

test("failed pause does not issue a resume", async () => {
  const requests: FileWriteControlRequest[] = [];
  let wrote = false;
  await assert.rejects(
    withPausedFileWrites(
      async (request) => {
        requests.push(request);
        throw new Error("pause failed");
      },
      async () => {
        wrote = true;
        return "unused";
      },
      async () => [],
    ),
    /pause failed/,
  );
  assert.equal(wrote, false);
  assert.deepEqual(requests, [{ fileWrites: "pause" }]);
});

test("failed mutation resumes file writes", async () => {
  const requests: FileWriteControlRequest[] = [];
  await assert.rejects(
    withPausedFileWrites(
      async (request) => {
        requests.push(request);
      },
      async () => {
        throw new Error("write failed");
      },
      async () => [],
    ),
    /write failed/,
  );
  assert.deepEqual(requests, [
    { fileWrites: "pause" },
    { fileWrites: "resume" },
  ]);
});
