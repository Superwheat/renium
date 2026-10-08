local fs = require("@lune/fs")
local luau = require("@lune/luau")
local scheduler = require("@lune/task")

local authorized = true
local added = {}
local leaves = 0
local addBehaviour = "return"
local players = {}
local services = {
	HttpService = { GenerateGUID = function() return "test-runtime" end },
	LogService = {
		MessageOut = { Connect = function() end },
		GetLogHistory = function() return {} end,
	},
	UserInputService = {},
	RunService = {
		RunState = "running",
		IsEdit = function() return false end,
		IsRunning = function() return true end,
	},
	Players = {
		LocalPlayer = { Name = "Player2" },
		GetPlayers = function() return players end,
	},
	StudioTestService = { EditModeActive = false },
}
function services.StudioTestService:AddPlayers(count)
	added[#added + 1] = count
	if addBehaviour == "error" then
		error("AddPlayers: can only be called from the server DataModel of a running Studio test session")
	elseif addBehaviour == "yield" then
		coroutine.yield()
	end
end
function services.StudioTestService:LeaveTest()
	leaves += 1
end
local runtimeGame = {
	GetService = function(_, name) return assert(services[name], name) end,
	GetAttribute = function() return nil end,
	SetAttribute = function() end,
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
local function runtime(role)
	return RuntimeApi.create({}, {
		runtimeId = role, bridgeRole = role,
		assertRequestLeaseActive = function() end,
		assertSessionOwnership = function()
			assert(authorized, "Renium session ownership was lost")
		end,
	})
end
local function named(count)
	local list = {}
	for index = 1, count do
		list[index] = { Name = `Player{index}` }
	end
	return list
end
local server, client, edit = runtime("play-server"), runtime("play-client"), runtime("edit")

players = named(2)
local result = server.startStopPlay({ addPlayers = 3 })
assert(result.ok and result.action == "addPlayers" and result.requested == 3, "server adds clients")
assert(#added == 1 and added[1] == 3 and #result.players == 2 and result.players[2] == "Player2")

players = named(7)
result = server.startStopPlay({ addPlayers = 2 })
assert(not result.ok and string.find(result.error, "add at most 1", 1, true), "cap counts current players")
players = named(8)
result = server.startStopPlay({ addPlayers = 1 })
assert(not result.ok and string.find(result.error, "--leave -p N", 1, true), "a full test says how to free a slot")
assert(#added == 1, "a refused request never calls AddPlayers")

players = named(1)
addBehaviour = "error"
result = server.startStopPlay({ addPlayers = 1 })
assert(not result.ok and string.find(result.error, "server DataModel", 1, true), "engine refusal is reported")
addBehaviour = "yield"
local startedAt = os.clock()
result = server.startStopPlay({ addPlayers = 1 })
assert(result.ok and os.clock() - startedAt < 1, "a slow AddPlayers does not hold the reply")
addBehaviour = "return"

for _, params in { { addPlayers = 0 }, { addPlayers = 1.5 }, { addPlayers = "2" } } do
	assert(not server.startStopPlay(params).ok, "invalid counts are refused")
end
result = edit.startStopPlay({ addPlayers = 1 })
assert(not result.ok and string.find(result.error, "play server", 1, true), "Edit cannot add clients")
result = client.startStopPlay({ addPlayers = 1 })
assert(not result.ok, "a client cannot add clients")

authorized = false
local ok, message = pcall(server.startStopPlay, { addPlayers = 1 })
assert(not ok and string.find(tostring(message), "ownership was lost", 1, true), "adding requires ownership")
ok, message = pcall(client.startStopPlay, { leave = true })
assert(not ok and string.find(tostring(message), "ownership was lost", 1, true), "leaving requires ownership")
players = named(3)
result = server.startStopPlay({ listPlayers = true })
assert(result.ok and result.action == "players" and #result.players == 3, "listing is read-only")
authorized = true

result = server.startStopPlay({ leave = true })
assert(not result.ok and string.find(result.error, "play client", 1, true), "the server cannot leave")
result = client.startStopPlay({ leave = true })
assert(result.ok and result.action == "leave" and result.player == "Player2", "client leaves")
assert(leaves == 0, "LeaveTest waits until the reply has been returned")
scheduler.wait()
assert(leaves == 1, "LeaveTest runs once the reply is out")
assert(#added == 3,"only accepted requests reached AddPlayers")
print("Renium play client add/leave tests passed")
