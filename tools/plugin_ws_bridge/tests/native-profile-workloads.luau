local fs = require("@lune/fs")
local luau = require("@lune/luau")
local serde = require("@lune/serde")
local now = 10
local Profile = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeConnection.module.luau"), {
	environment = { os = { clock = function() return now end } },
})()
Profile.profileStart(10)
now = 10.001
Profile.profileStep("queue", 10)
local worker = coroutine.create(function()
	Profile.profileStart(now)
	now += 0.003
	Profile.profileStep("worker", 10.001)
	local result = Profile.profileFinish()
	assert(#result.spans == 1 and result.spans[1][1] == "worker")
end)
assert(coroutine.resume(worker))
now = 10.006
Profile.profileStep("handler", 10.001, { instances = 42 })
local result = Profile.profileFinish()
assert(result.version == 1 and #result.spans == 2)
assert(math.abs(result.elapsedMs - 6) < 0.00001)
assert(math.abs(result.spans[2][3] - result.spans[2][2] - 5) < 0.00001)
assert(result.spans[2][4].instances == 42)
assert(Profile.profileFinish() == nil)
Profile.profileStep("disabled", now)
assert(Profile.profileFinish() == nil)
assert(Profile.profileWaitObserver("disabled") == nil)
Profile.profileStart(now)
local observe = assert(Profile.profileWaitObserver("scheduler yield"))
local child = coroutine.create(function()
	local started = now
	now += 0.020
	observe(started)
end)
assert(coroutine.resume(child))
local waits = Profile.profileFinish()
assert(#waits.spans == 1 and waits.spans[1][1] == "scheduler yield")
assert(math.abs(waits.spans[1][3] - waits.spans[1][2] - 20) < 0.00001)
Profile.profileStart(now)
local observeSlice, finishSlice = Profile.profileWaitObserver("wait", "compute")
now += 0.008
local waitStarted = now
now += 0.030
observeSlice(waitStarted)
now += 0.004
finishSlice()
local slices = Profile.profileFinish()
assert(#slices.spans == 3)
for index, expected in ipairs({ { "compute", 0, 8 }, { "wait", 8, 38 }, { "compute", 38, 42 } }) do
	local span = slices.spans[index]
	assert(span[1] == expected[1])
	assert(math.abs(span[2] - expected[2]) < 0.00001 and math.abs(span[3] - expected[3]) < 0.00001,
		"Active overlay slices and waits must cover the real intervals without overlap")
end

local sent
local Transport = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeTransport.module.luau"), {
	environment = { game = { GetService = function() return {
		JSONEncode = function(_, value) return serde.encode("json", value) end,
	} end } },
})()
local client = { Send = function(_, message) sent = message end }
local chunk = { start = 1, nextStart = 6, total = 5, chunk = "a\nb\0c", nativeProfile = result }
assert(Transport.sendSuccessResponse(1, client, 12, "getEditorBinaryOverlayChunk", chunk, 6))
local separator = assert(sent:find("\n", 1, true))
assert(sent:sub(1, 5) == "RBS5 ")
assert(sent:sub(separator + 1) == chunk.chunk)
local header = serde.decode("json", sent:sub(6, separator - 1))
assert(header.id == 12 and header.nativeProfile.spans[2][4].instances == 42)
chunk.nativeProfile = nil
assert(Transport.sendSuccessResponse(1, client, 12, "getEditorBinaryOverlayChunk", chunk, 6))
assert(sent:sub(1, 5) == "RBS2 ", "legacy hosts must keep the existing wire format")
print("Renium native profile clock and transport tests passed")
