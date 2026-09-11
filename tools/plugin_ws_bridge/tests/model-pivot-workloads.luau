local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")

local function extract(file, first, after, environment, returned)
	local source = fs.readFile(file)
	local start = assert(source:find(first, 1, true))
	local finish = assert(source:find(after, start, true))
	return luau.load(source:sub(start, finish - 1) .. "\nreturn " .. returned, { environment = environment })()
end

local readers = {
	extract("tools/plugin_ws_bridge/BridgeEditorSync.module.lua", "local function readProperty(", "local function candidateBucketsForParent(", {}, "readProperty"),
	extract("tools/plugin_ws_bridge/BridgePluginRuntime.module.lua", "local function tryReadModelPivotProperty(", "local function tryRead(", {}, "tryReadModelPivotProperty"),
}
local custom = luau.load(fs.readFile("tools/plugin_ws_bridge/rbx_dom_lua/customProperties.module.lua"), {
	environment = {
		game = { GetService = function() return {} end },
		Enum = roblox.Enum,
		script = { Parent = { Error = {} } },
		require = function(value) return value end,
	},
})()
table.insert(readers, function(instance, property) return custom.Model[property].read(instance) end)

for _, class in { "Model", "WorldModel" } do
	local saved = roblox.CFrame.new(1, 2, 3)
	local primary = roblox.CFrame.new(40, 50, 60)
	local model = {
		WorldPivot = saved,
		IsA = function(_, name) return name == class end,
		GetScale = function() return 2 end,
	}
	for _, primaryPart in { false, true } do
		model.GetPivot = function() return if primaryPart then primary else saved end
		for _, read in readers do
			for _, property in { "WorldPivot", "WorldPivotData", "Origin", "Scale" } do
				local ok, value = read(model, property)
				local expected = if property == "Scale" then 2 elseif property == "Origin" then model:GetPivot() else saved
				assert(ok and value == expected, `Wrong {class}.{property} with PrimaryPart={primaryPart}`)
			end
		end
	end
end
print("Model pivots: export, transactional readback and custom codec keep saved pivots separate from PrimaryPart origin")

local codec = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeValueCodec.module.lua"), {
	environment = { game = { GetService = function() return {} end } },
})()
local equality = require("../BridgeValueEquality.module")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.lua")
local loopStart = assert(source:find("local postCommitPropertyChanges = session.postCommitPropertyChanges", 1, true))
local start = assert(source:find('local okRead, current = readProperty(instance, "WorldPivot")', loopStart, true))
local finish = assert(source:find("local okDecode, value = decodePropertyValue(", start, true))
local matches = luau.load("return function(instance, raw) for _=1,1 do local change={properties={WorldPivot=raw}} "
	.. source:sub(start, finish - 1) .. " return false end return true end", {
	environment = {
		readProperty = function(instance) return true, instance end,
		BridgeValueCodec = codec,
		BridgeValueEquality = equality,
	},
})()
local components = { 1, 2, 3, 1, 0, 0, 0, 1, 2.2958874039497803e-41, 0, 0, 1 }
local native = { GetComponents = function() return table.unpack(components) end }
local raw = { _type = "CFrame", components = table.clone(components) }
assert(matches(native, raw), "unchanged subnormal must not be rewritten")
raw.components[9] = 0
assert(not matches(native, raw), "component comparison must remain exact")
raw.components[1] = 10
assert(not matches(native, raw), "real pivot edits must still apply")
assert(not matches(native, {}), "invalid values must go through the decoder")
print("Native pivots: exact serialized comparison avoids lossy CFrame reconstruction; real edits remain writes")
