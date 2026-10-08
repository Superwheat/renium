local fs = require("@lune/fs")
local luau = require("@lune/luau")
local scheduler = require("@lune/task")

local running = false
local authorized = false
local starts = 0
local stops = 0
local writes = 0
local ownershipChecks = 0
local attributes = {}
local services = {
	HttpService = { GenerateGUID = function() return "test-runtime" end },
	LogService = {
		MessageOut = { Connect = function() end },
		GetLogHistory = function() return {} end,
	},
	UserInputService = {},
	RunService = {
		RunState = "stopped",
		IsEdit = function() return not running end,
		IsRunning = function() return running end,
	},
	StudioTestService = {
		EditModeActive = true,
		CanLeaveTest = function() return false end,
	},
}
function services.StudioTestService:ExecutePlayModeAsync()
	starts += 1
	running = true
	coroutine.yield()
end
function services.StudioTestService:EndTest()
	stops += 1
	running = false
end
function services.RunService:Stop()
	running = false
end
local runtimeGame = {
	GetService = function(_, name) return assert(services[name], name) end,
	GetAttribute = function(_, name) return attributes[name] end,
	SetAttribute = function(_, name, value)
		writes += 1
		attributes[name] = value
	end,
}
local codec = {}
local RuntimeApi = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeRuntimeApi.module.luau"), {
	environment = {
		game = runtimeGame,
		Enum = { RunState = { Stopped = "stopped" } },
		task = scheduler,
		script = { Parent = { BridgeValueCodec = codec } },
		require = function(module)
			assert(module == codec)
			return module
		end,
	},
})()
local api = RuntimeApi.create({}, {
	runtimeId = "test-runtime", bridgeRole = "edit",
	assertRequestLeaseActive = function() end,
	assertSessionOwnership = function()
		ownershipChecks += 1
		assert(authorized, "Renium session ownership was lost")
	end,
})

local status = api.startStopPlay({})
assert(status.ok and not status.running and status.readyForStart)
assert(ownershipChecks == 0 and writes == 0 and starts == 0 and stops == 0,
	"readiness must not require a mutation token or change Studio")
for _, params in { { start = true }, { stop = true } } do
	local ok, message = pcall(api.startStopPlay, params)
	assert(not ok and string.find(tostring(message), "ownership was lost", 1, true))
end
assert(starts == 0 and stops == 0 and writes == 0, "mutations still require ownership")
authorized = true
status = api.startStopPlay({ start = true, launchNonce = "test-launch" })
assert(status.ok and status.running and starts == 1)
assert(status.engineRunning and status.engineStarted and status.executing, "Studio entered the test")
assert(status.mode == "play" and status.players == nil)
authorized = false
local checksBefore, writesBefore = ownershipChecks, writes
status = api.startStopPlay({})
assert(status.ok and status.running and status.launchNonce == "test-launch")
assert(ownershipChecks == checksBefore and writes == writesBefore and stops == 0,
	"observing active Play must preserve its ownership and attributes")
authorized = true
status = api.startStopPlay({ stop = true })
assert(status.ok and not status.running and stops == 1)
assert(attributes.__ReniumLaunchNonce == nil and attributes.__ReniumEditRuntimeId == nil)
api.cleanup()

-- Studio can return from a start while the previous test is still ending, without
-- ever opening the test; the status must tell that apart from a test still coming up.
local launched
function services.StudioTestService:ExecuteMultiplayerTestAsync(players, testArgs)
	launched = { players = players, args = testArgs }
end
runtimeGame.Name = "scratch.rbxl"
local multi = RuntimeApi.create({}, {
	runtimeId = "test-runtime", bridgeRole = "edit",
	assertRequestLeaseActive = function() end,
	assertSessionOwnership = function() end,
})
status = multi.startStopPlay({ start = true, players = 2, launchNonce = "multi-launch" })
assert(status.ok and status.mode == "multi" and status.players == 2 and status.launchNonce == "multi-launch")
local renium = launched.args.__renium
assert(launched.players == 2 and renium.nonce == "multi-launch" and renium.editRuntimeId == "test-runtime")
assert(renium.placeName == "scratch.rbxl", "test DataModels learn the Edit place name")
status = multi.startStopPlay({})
assert(status.running and status.starting and not status.readyForStart)
assert(status.engineRunning == false and status.engineStarted == false and status.executing == false,
	"a start Studio returned from without a test is visible")
status = multi.startStopPlay({ stop = true, launchNonce = "multi-launch" })
assert(status.ok and not status.running and status.mode == nil and status.players == nil)
assert(not status.readyForStart, "the controller settles before the next start")
multi.cleanup()

function services.StudioTestService:ExecuteRunModeAsync()
	error("Studio is busy")
end
local refused = RuntimeApi.create({}, {
	runtimeId = "test-runtime", bridgeRole = "edit",
	assertRequestLeaseActive = function() end,
	assertSessionOwnership = function() end,
})
status = refused.startStopPlay({ start = true, mode = "run", launchNonce = "run-launch" })
assert(not status.running and not status.starting and status.launchNonce == nil)
assert(string.find(status.lastError, "Studio is busy", 1, true), "a refused start keeps Studio's error for the daemon")
assert(status.engineStarted == false and status.executing == false)
refused.cleanup()
print("Renium play observation and ownership tests passed")
