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
local RuntimeApi = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeRuntimeApi.module.lua"), {
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
print("Renium play observation and ownership tests passed")
