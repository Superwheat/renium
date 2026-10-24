local fs = require("@lune/fs")
local luau = require("@lune/luau")
local created, delayed, frames = {}, {}, {}
local function new(class)
	local object = { ClassName = class, Rotation = 0 }
	function object:Destroy() self.destroyed = true; self.Parent = nil end
	created[#created + 1] = object
	return object
end
local function tuple(...) return { ... } end
local playerGui = {}
local module = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeTeleportGui.module.luau"), { environment = {
	Instance = { new = new }, Color3 = { fromRGB = tuple }, Vector2 = { new = tuple },
	UDim = { new = tuple }, UDim2 = { new = tuple, fromScale = tuple, fromOffset = tuple },
	Enum = { Font = { GothamMedium = "font" } },
	task = { delay = function(_, callback) delayed[#delayed + 1] = callback end },
	game = { GetService = function(_, name)
		if name == "Players" then return { LocalPlayer = { WaitForChild = function() return playerGui end } } end
		assert(name == "RunService")
		return { RenderStepped = { Connect = function(_, callback)
			local connection = { callback = callback, connected = true }
			function connection:Disconnect() self.connected = false end
			frames[#frames + 1] = connection
			return connection
		end } }
	end },
} })()
local gui = module.create()
gui.show({ placeId = 12 })
local screen = gui.get()
assert(screen.Parent == playerGui and screen.ResetOnSpawn == false)
assert(#frames == 1)
frames[1].callback(0.25)
local image, spinner
for _, object in created do
	if object.ClassName == "ImageLabel" then image = object end
	if object.Rotation == 60 then spinner = object end
end
assert(image.Image == "rbxthumb://type=Asset&id=12&w=420&h=420" and spinner)
local detail
for _, object in created do
	if object.Text == "Checking saved version and access…" then detail = object end
end
assert(detail)
local note
for _, object in created do
	if object.Text == "STUDIO EMULATION · Stop Play to return to your source place" then note = object end
end
assert(note)
gui.show({ placeId = 12, mode = "checking-native" })
assert(note.Text == "CHECKING NATIVE PLAYTEST · Stop Play to return to your source place")
gui.show({ placeId = 12, mode = "native" })
assert(note.Text == "NATIVE PLAYTEST · Stop Play to return to your source place")
gui.show({ placeId = 12, mode = "emulated", nativeRequested = true })
assert(note.Text == "STUDIO EMULATION · native unavailable · Stop Play to return to your source place")
gui.show({ placeId = 12, version = 10, contentSource = "cache" })
assert(detail.Text == "Saved version 10 · from cache")
gui.show({ placeId = 12, version = 11, contentSource = "download" })
assert(detail.Text == "Saved version 11 · downloaded")
gui.fail("Download denied", false)
assert(not frames[1].connected and #delayed == 1 and spinner.Visible == false)
gui.show({ placeId = 13 })
assert(gui.get() == screen and #frames == 2 and spinner.Visible)
delayed[1]()
assert(not screen.destroyed and gui.get() == screen, "old error timeout must not dismiss a new teleport")
gui.arrive()
assert(gui.get() == nil and screen.Parent == nil and not screen.destroyed and not frames[2].connected)
gui.show({ placeId = 14 })
assert(screen.destroyed, "arriving GUI is retired on the next travel")
gui.fail("Commit response lost", true)
assert(#delayed == 1 and gui.get(), "uncertain commit retains the failure screen until Stop Play")
gui.dismiss()
assert(gui.get() == nil and not frames[#frames].connected)
print("teleport loading GUI lifecycle workloads passed")
