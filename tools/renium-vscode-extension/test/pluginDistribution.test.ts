import assert from "node:assert/strict";
import { test } from "node:test";

import { isRobloxModel } from "../src/pluginDistribution";

test("Roblox model validation accepts binary and XML headers", () => {
  assert.equal(isRobloxModel(Buffer.from("<roblox!\x89\xff\r\n\x1a\n\x00\x00\x00\x00", "latin1")), true);
  assert.equal(isRobloxModel(Buffer.from('<roblox version="4">', "utf8")), true);
  assert.equal(isRobloxModel(Buffer.from("<html>not a model</html>", "utf8")), false);
});
