local fs = require("@lune/fs")
local luau = require("@lune/luau")
local created = {}
local coreGui, scriptContext, replicatedFirst = {}, {}, {}
local module = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeTeleportGui.module.luau"), { environment = {
	Instance = { new = function(class)
		assert(class == "BindableFunction", "Roblox owns the loading UI; Renium must not create a replacement")
		local object = { ClassName = class }
		function object:Destroy() self.destroyed = true; self.Parent = nil end
		created[#created + 1] = object
		return object
	end },
	game = { GetService = function(_, name)
		if name == "ScriptContext" then return scriptContext end
		if name == "ReplicatedFirst" then return replicatedFirst end
		assert(name == "CoreGui")
		return coreGui
	end },
} })()
local gui = module.create("client-a")
gui.show({ id = 1, placeId = 12 })
assert(gui.get() == nil and #created == 1)
local relay = created[1]
assert(relay.Parent == coreGui and relay.Name == "__ReniumTeleportGui_client-a")
local args = table.pack(relay.OnInvoke())
assert(args.n == 3 and args[1] == scriptContext and args[2] == coreGui and args[3] == replicatedFirst)
local notification = gui.native()
assert(notification.show and notification.path[1] == "CoreGui" and notification.path[2] == relay.Name)
assert(gui.native() == nil, "each notification is consumed once")
gui.show({ id = 1, placeId = 12, version = 4 })
assert(gui.native() == nil, "download progress must not restart the engine's loading screen")
gui.fail("Download denied", false)
assert(gui.native().show == false)
gui.show({ id = 2, placeId = 13 })
assert(#created == 1 and gui.native().show == true)
args = table.pack(relay.OnInvoke())
assert(args.n == 3 and args[1] == scriptContext and args[2] == coreGui and args[3] == replicatedFirst)
gui.arrive()
assert(gui.native().show == false)
gui.dismiss()
assert(relay.destroyed and gui.native() == nil)
gui.show({ id = 3, placeId = 14 })
assert(#created == 2 and not created[2].destroyed)
gui.dismiss()
print("Roblox teleport CoreGui notification lifecycle workloads passed")
