local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")

local game = roblox.Instance.new("DataModel")
local storage = roblox.Instance.new("ServerStorage")
storage.Parent = game
local players = roblox.Instance.new("Players")
players.Parent = game
local function create(className, name, parent)
	local value = roblox.Instance.new(className)
	value.Name = name
	value.Parent = parent
	return value
end
local noSignal = { Connect = function() end }
local storageProxy = { ChildAdded = noSignal, ChildRemoved = noSignal, GetChildren = function() return {} end }
local module = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeSessionLock.module.lua"), {
	environment = { game = { GetService = function(_, name)
		return if name == "ServerStorage" then storageProxy else players
	end } },
})()
local lock = module.create("test", function() error("unexpected ownership change") end, false)
local root = create("Folder", "Ordinary", storage)
local leaf = root
for index = 1, 200 do leaf = create("Folder", `Level{index}`, leaf) end
assert(not lock.isLockInstance(leaf))
local marker = create("Folder", "__Renium_SessionLock", storage)
marker:SetAttribute("ReniumSessionLock", true)
root.Parent = marker
assert(lock.isLockInstance(root) and lock.isLockInstance(leaf))
local collision = create("Folder", "__Renium_SessionLock", root)
leaf.Parent = collision
assert(lock.isLockInstance(leaf), "an unmarked same-name folder must not hide the marked ancestor")
marker:SetAttribute("ReniumSessionLock", false)
assert(not lock.isLockInstance(leaf), "marker edits must not leave a cached exclusion")
local objectLock = create("ObjectValue", "__Renium_SessionLock", storage)
root.Parent = objectLock
assert(lock.isLockInstance(leaf) and lock.isLockInstance(objectLock))
root.Parent = storage
assert(not lock.isLockInstance(leaf), "moving out must immediately clear the exclusion")
objectLock.Name = "OrdinaryObject"
assert(not lock.isLockInstance(objectLock))
root.Parent = nil
assert(not lock.isLockInstance(leaf))
game:Destroy()
root:Destroy()
print("Session lock filtering: deep trees, name collisions, marker edits and reparenting passed")
