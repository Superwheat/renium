import assert from "node:assert/strict";
import { test } from "node:test";

import { isRobloxModel, reniumPluginReleaseUrl } from "../src/pluginDistribution";

test("plugin download URL is pinned to the extension version", () => {
  assert.equal(
    reniumPluginReleaseUrl("0.1.2"),
    "https://github.com/Superwheat/renium/releases/download/v0.1.2/Renium.rbxm",
  );
  assert.throws(() => reniumPluginReleaseUrl("latest"), /Invalid Renium extension version/);
});

test("Roblox model validation accepts binary and XML headers", () => {
  assert.equal(isRobloxModel(Buffer.from("<roblox!\x89\xff\r\n\x1a\n\x00\x00\x00\x00", "latin1")), true);
  assert.equal(isRobloxModel(Buffer.from('<roblox version="4">', "utf8")), true);
  assert.equal(isRobloxModel(Buffer.from("<html>not a model</html>", "utf8")), false);
});
