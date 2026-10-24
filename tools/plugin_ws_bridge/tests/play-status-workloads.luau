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
local execution
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
	execution = coroutine.running()
	coroutine.yield()
end
function services.StudioTestService:EndTest()
	stops += 1
	running = false
	if execution and coroutine.status(execution) == "suspended" then
		assert(coroutine.resume(execution))
	end
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
assert(status.teleportJournal.nonce == "test-launch" and status.teleportJournal.ready)
assert(api.startStopPlay({ resumeTeleport = true, launchNonce = "test-launch" }).ok,
	"an idle reconnect can retain the same controller")
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

-- A destination launch is owned by the original Edit controller while its
-- source Play runtimes are gone. An ordinary Stop retires that permission.
status = api.startStopPlay({ start = true, launchNonce = "travel-source" })
assert(status.running)
local reserved = api.startStopPlay({ stop = true, launchNonce = "travel-source", teleportTransition = "hop-1", prepareTeleport = true })
assert(reserved.ok and reserved.running and reserved.teleportTransition == "hop-1", "reserving a transition must retain the source test")
services.StudioTestService:EndTest()
local stopped = api.startStopPlay({})
local stopDeadline = os.clock() + 1
while stopped.running and os.clock() < stopDeadline do
	scheduler.wait()
	stopped = api.startStopPlay({})
end
assert(stopped.ok and not stopped.running and stopped.teleportTransition == "hop-1")
local stopCount = stops
assert(api.startStopPlay({ stop = true, launchNonce = "travel-source", teleportTransition = "hop-1" }).ok)
assert(stops == stopCount, "a repeated transition stop must not stop another launch")
status = api.startStopPlay({ start = true, launchNonce = "travel-destination", teleportTransition = "hop-1" })
assert(status.ok and status.running and status.launchNonce == "travel-destination")
assert(not status.teleportJournal.ready, "starting a destination is not an arrival receipt")
assert(not api.startStopPlay({ resumeTeleport = true, launchNonce = "travel-destination" }).ok,
	"reconnect must not adopt an unacknowledged destination")
assert(not api.startStopPlay({ completeTeleport = true, launchNonce = "travel-destination", teleportTransition = "stale" }).ok)
assert(api.startStopPlay({ completeTeleport = true, launchNonce = "travel-destination", teleportTransition = "hop-1" }).ok)
api.requestCancellation()
assert(api.startStopPlay({}).teleportJournal.ready, "the Edit receipt survives a daemon connection")
assert(not api.startStopPlay({ resumeTeleport = true, launchNonce = "replaced" }).ok)
assert(api.startStopPlay({ resumeTeleport = true, launchNonce = "travel-destination" }).ok,
	"a replacement daemon can reclaim only the completed recorded launch")
local stale = api.startStopPlay({ stop = true, launchNonce = "travel-source", teleportTransition = "hop-1" })
assert(not stale.ok and api.startStopPlay({}).running, "old source replies cannot stop the destination")
stopped = api.startStopPlay({ stop = true, launchNonce = "travel-destination", teleportTransition = "hop-2" })
assert(stopped.ok and stopped.teleportTransition == "hop-2")
assert(api.startStopPlay({ stop = true }).ok)
assert(api.startStopPlay({}).teleportJournal == nil, "ordinary Stop retires the controller receipt")
local startsBefore = starts
status = api.startStopPlay({ start = true, launchNonce = "late-destination", teleportTransition = "hop-2" })
assert(not status.ok and starts == startsBefore and not running, "Stop cancels a delayed destination start")
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
