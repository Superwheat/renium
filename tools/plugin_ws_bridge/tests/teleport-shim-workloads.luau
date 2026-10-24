local fs = require("@lune/fs")
local luau = require("@lune/luau")
local serde = require("@lune/serde")

local studio, client, edit = true, false, false
local invoked
local original = { SourcePlaceId = 0, Members = { 101 } }
local player = { Name = "Player1", GetJoinData = function() return original end }
local context = { mode = "emulated", placeId = 12, sourcePlaceId = 11, originPlaceId = 10,
	gameId = 7, jobId = "original-job", data = { Player1 = { teleportData = { marker = "saved" } } } }
local arriving = {}
local services = {
	RunService = { IsStudio = function() return studio end, IsClient = function() return client end,
		IsServer = function() return not client end, IsRunning = function() return not edit end,
		IsEdit = function() error("game scripts cannot call Plugin-only IsEdit") end },
	HttpService = { JSONDecode = function(_, value) return serde.decode("json", value) end },
	Players = { LocalPlayer = player },
}
local shim = luau.load(fs.readFile("tools/plugin_ws_bridge/TeleportShim.module.luau"), { environment = {
	game = { GetService = function(_, name) return assert(services[name]) end },
	script = {
		WaitForChild = function(_, name)
			if name == "Request" then return { Invoke = function(_, ...) invoked = { ... }; return { id = 1 } end } end
			assert(name == "Context"); return { Value = serde.encode("json", context) }
		end,
		FindFirstChild = function(_, name) assert(name == "ArrivingGui"); return { Value = arriving } end,
	},
} })()
local join = shim.GetJoinData(player)
assert(shim.Teleport(12, { player }, nil).id == 1 and invoked[1] == 12 and invoked[2][1] == player)
edit = true
assert(not pcall(shim.Teleport, 12, { player }))
edit = false
assert(join.Emulated and join.SourcePlaceId == 11 and join.SourceGameId == 7)
assert(join.TeleportData.marker == "saved" and join.Members == original.Members)
assert(original.SourcePlaceId == 0 and original.TeleportData == nil, "the shim must not alter engine join data")
assert(shim.GetContext().originPlaceId == 10 and shim.GetContext().jobId == "original-job")
assert(not pcall(shim.GetLocalPlayerTeleportData) and not pcall(shim.GetArrivingTeleportGui))
client = true
assert(not pcall(shim.Teleport, 12, { player }), "clients cannot invoke the server-only interim API")
assert(shim.GetLocalPlayerTeleportData().marker == "saved" and shim.GetArrivingTeleportGui() == arriving)
context.data.Player1 = nil
assert(shim.GetLocalPlayerTeleportData() == nil)
context.mode = "source"
assert(shim.GetJoinData(player) == original, "initial API preserves ordinary join data")
studio = false
assert(not pcall(shim.GetContext))
print("teleport shim workloads passed")
