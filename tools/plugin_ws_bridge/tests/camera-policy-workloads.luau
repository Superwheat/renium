local fs = require("@lune/fs")
local luau = require("@lune/luau")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.lua")

local function section(first, after)
	local start = assert(source:find(first, 1, true))
	local finish = assert(source:find(after, start, true))
	return source:sub(start, finish - 1)
end

local policy = section("local function isProtectedWorkspaceCameraInstance(", "local function decodeRefValue(")
local upsert = section("local function syncDesiredEntry(", "local function removeUnknownInstances(")
local workspace = {}
local resolved, remembered
local placements = 0
local sync, protected = luau.load(policy .. upsert .. "\nreturn syncDesiredEntry, isProtectedWorkspaceCameraInstance", {
	environment = {
		Workspace = workspace,
		liveInstance = function(instance) return instance end,
		resolveEntryInstance = function() return resolved end,
		resolveEntryParent = function() return workspace end,
		Instance = { new = function(class) return { ClassName = class } end },
		setParentForSync = function(instance, parent) instance.Parent = parent end,
		syncEntryPlacement = function() placements += 1 end,
		rememberEntryResolution = function(_, _, instance) remembered = instance end,
	},
})()

for _, name in { "Camera", "CurrentCamera", "Renamed viewport" } do
	local viewport = { ClassName = "Camera", Name = name, Parent = workspace }
	local extra = { ClassName = "Camera", Name = name, Parent = workspace }
	workspace.CurrentCamera = viewport
	assert(protected(viewport) and not protected(extra) and not protected(nil))
	local entry = { key = name, className = "Camera", pathSegments = { "Workspace", name } }
	for _, case in { { instance = viewport, protected = true }, { instance = extra }, {} } do
		resolved = case.instance
		remembered = nil
		placements = 0
		local stats = { noops = 0, instanceCreated = 0 }
		local entries = {}
		local result = sync(entry, "Workspace", {}, stats, entries, {}, true)
		assert(result ~= nil and entries[entry.key] == result and remembered == result)
		if case.protected then
			assert(result == viewport and placements == 0 and stats.noops == 1)
		elseif case.instance then
			assert(result == extra and placements == 1 and stats.noops == 0)
		else
			assert(result ~= viewport and result.Parent == workspace and result.Name == name)
			assert(stats.instanceCreated == 1 and stats.noops == 0)
		end
	end
end

print("Viewport policy: active identity retained; same-named camera upserts and creations preserved")
