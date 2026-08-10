import assert from "node:assert/strict";
import { test } from "node:test";

import {
  buildCommitMessage,
  nameStatusAffectedPaths,
  parseAheadBehind,
  parseNameStatusZ,
  parsePorcelainV1Z,
  redactRemoteUrl,
  remoteUrlToWebUrl,
  renderGitArgs,
  shouldPullFromStudioBeforePush,
  summarizeStatus,
} from "../src/gitSync";

test("parsePorcelainV1Z classifies tracked, untracked, renamed, deleted, and conflicted entries", () => {
  const entries = parsePorcelainV1Z([
    " M src/Foo.server.lua",
    " D src/Deleted.lua",
    "?? src/New.client.lua",
    "R  src/NewName.lua",
    "src/OldName.lua",
    "UU src/Conflict.lua",
    "!! temp/generated.bin",
    "",
  ].join("\0"));

  assert.equal(entries.length, 6);
  assert.deepEqual(entries.map((entry) => entry.path), [
    "src/Foo.server.lua",
    "src/Deleted.lua",
    "src/New.client.lua",
    "src/NewName.lua",
    "src/Conflict.lua",
    "temp/generated.bin",
  ]);
  assert.equal(entries[0].kind, "modified");
  assert.equal(entries[0].unstaged, true);
  assert.equal(entries[1].kind, "deleted");
  assert.equal(entries[1].deleted, true);
  assert.equal(entries[2].untracked, true);
  assert.equal(entries[3].kind, "renamed");
  assert.equal(entries[3].originalPath, "src/OldName.lua");
  assert.equal(entries[4].conflicted, true);
  assert.equal(entries[5].ignored, true);

  assert.deepEqual(summarizeStatus(entries), {
    total: 6,
    tracked: 4,
    staged: 2,
    unstaged: 3,
    untracked: 1,
    ignored: 1,
    conflicted: 1,
    deleted: 1,
  });
});

test("parseNameStatusZ handles rename records and ordinary records", () => {
  const entries = parseNameStatusZ(["M", "src/A.lua", "R100", "src/Old.lua", "src/New.lua", "D", "src/Gone.lua", ""].join("\0"));
  assert.deepEqual(entries, [
    { status: "M", path: "src/A.lua" },
    { status: "R100", originalPath: "src/Old.lua", path: "src/New.lua" },
    { status: "D", path: "src/Gone.lua" },
  ]);
});

test("nameStatusAffectedPaths includes deleted and renamed source targets", () => {
  assert.deepEqual(nameStatusAffectedPaths([
    { status: "D", path: "src/Gone.lua" },
    { status: "R100", originalPath: "src/Old.lua", path: "src/New.lua" },
    { status: "C100", originalPath: "src/Template.lua", path: "src/Copy.lua" },
    { status: "M", path: "src/New.lua" },
  ]), [
    "src/Gone.lua",
    "src/Old.lua",
    "src/New.lua",
    "src/Copy.lua",
  ]);
});

test("parseAheadBehind decodes rev-list left/right counts", () => {
  assert.deepEqual(parseAheadBehind("3\t5\n"), { ahead: 3, behind: 5 });
  assert.deepEqual(parseAheadBehind("bad"), { ahead: 0, behind: 0 });
});

test("remote URL helpers redact credentials and convert Git remotes to browser URLs", () => {
  const secretUrl = "https://ghp_SECRET123@github.com/owner/repo.git?token=abc123";
  const redacted = redactRemoteUrl(secretUrl);
  assert.match(redacted, /https:\/\/\*\*\*@github\.com\/owner\/repo\.git/);
  assert.doesNotMatch(redacted, /SECRET123|abc123/);
  assert.equal(remoteUrlToWebUrl(secretUrl), "https://github.com/owner/repo");
  assert.equal(remoteUrlToWebUrl("git@github.com:owner/repo.git"), "https://github.com/owner/repo");
  assert.equal(remoteUrlToWebUrl("ssh://git@github.com/owner/repo.git"), "https://github.com/owner/repo");
});

test("renderGitArgs redacts secret-bearing arguments", () => {
  const rendered = renderGitArgs(["push", "https://token@example.com/owner/repo.git", "hello world"]);
  assert.equal(rendered, "push https://***@example.com/owner/repo.git \"hello world\"");
});

test("buildCommitMessage expands Renium Git sync placeholders", () => {
  const message = buildCommitMessage("Sync ${branch} on ${date} at ${datetime}", "feature/git-sync");
  assert.match(message, /^Sync feature\/git-sync on \d{4}-\d{2}-\d{2} at \d{4}-\d{2}-\d{2}T/);
  assert.doesNotMatch(message, /\$\{/);
});

test("Git commit and push only pulls Studio when configured or selected", () => {
  assert.equal(shouldPullFromStudioBeforePush("always", false), true);
  assert.equal(shouldPullFromStudioBeforePush("never", false), false);
  assert.equal(shouldPullFromStudioBeforePush("ask", false, "pull"), true);
  assert.equal(shouldPullFromStudioBeforePush("ask", false, "current"), false);
  assert.equal(shouldPullFromStudioBeforePush("never", true), true);
});
