local fs = require("@lune/fs")
local luau = require("@lune/luau")
local task = require("@lune/task")

local edit = true
local playing = false
local calls = 0
local active = true
local ownsSession = true
local failure = false
local pending = false
local game = { PlaceId = 456, GameId = 123 }
local services = {
	AssetService = {
		SavePlaceAsync = function(_, parameters)
			assert(parameters.PlaceId == 456 and parameters.SaveWithoutPublish == false)
			calls += 1
			if failure then error("HTTP 403: denied") end
			if pending then coroutine.yield() end
		end,
	},
}
function game:GetService(name) return assert(services[name], name) end
local Publish = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgePublish.module.luau"), {
	environment = { game = game },
})()
local context = {
	runtimeId = "edit-1",
	isPlayModeRunning = function() return not edit or playing end,
	assertRequestLeaseActive = function() assert(active, "cancelled") end,
	assertSessionOwnership = function() assert(ownsSession, "ownership lost") end,
}
local parameters = { runtimeId = "edit-1", gameId = 123, placeId = 456, dryRun = true }
assert(Publish.run(parameters, context).published == false and calls == 0)
parameters.dryRun = false
local function refuses()
	local before = calls
	assert(not pcall(Publish.run, parameters, context))
	assert(calls == before, "preflight rejection must not call Roblox")
end
edit = false
refuses()
edit = true
playing = true
refuses()
playing = false
game.PlaceId = 0
refuses()
game.PlaceId = 457
refuses()
game.PlaceId = 456
parameters.runtimeId = "replaced"
refuses()
parameters.runtimeId = "edit-1"
active = false
refuses()
active = true
ownsSession = false
refuses()
ownsSession = true
failure = true
local rejected = Publish.run(parameters, context)
assert(not rejected.ok and string.find(rejected.error, "HTTP 403", 1, true))
assert(calls == 1, "failed publish must not retry")
failure = false
pending = true
local result
local worker = coroutine.create(function() result = Publish.run(parameters, context) end)
assert(coroutine.resume(worker))
assert(result == nil and calls == 2, "acceptance is not completion")
assert(coroutine.resume(worker))
assert(result.ok and result.published and result.placeId == 456 and calls == 2)

local runtime = fs.readFile("tools/plugin_ws_bridge/BridgePluginRuntime.module.luau")
for _, section in { "bridgeExclusiveMethods", "bridgeReplayProtectedMethods" } do
	local start = assert(string.find(runtime, `Config.{section} = \{`, 1, true))
	local finish = assert(string.find(runtime, "\n\t}", start, true))
	assert(string.find(string.sub(runtime, start, finish), "publishPlace = true", 1, true))
end
task.wait()
print("Publish safeguards and delayed completion passed")
