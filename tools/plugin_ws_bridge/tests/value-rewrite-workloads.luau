local fs = require("@lune/fs")
local luau = require("@lune/luau")

-- Lune has no engine Content type; use its public SourceType/Uri contract.
local kinds = { None = "None", Uri = "Uri", Object = "Object" }
local equality = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeValueEquality.module.luau"), {
	environment = {
		Enum = { ContentSourceType = kinds },
		typeof = function(value)
			return if type(value) == "table" and value.SourceType then "Content" else typeof(value)
		end,
	},
})()
local function content(uri)
	return { SourceType = kinds.Uri, Uri = uri }
end
local canonical = "rbxassetid://123"
for _, spelling in { "http://www.roblox.com/asset/?id=123", "https://ROBLOX.com/asset?id=123", "RBXASSETID://123" } do
	assert(equality.valuesEqual(content(canonical), spelling), spelling)
	assert(equality.valuesEqual(spelling, content(canonical)), spelling)
	assert(equality.valuesEqual(content(canonical), content(spelling)), `Content to Content: {spelling}`)
end
for _, other in { "rbxassetid://124", "https://ww.roblox.com/asset?id=123", "https://roblox.com.evil/asset?id=123", "rbxassetid://123&version=2" } do
	assert(not equality.valuesEqual(content(canonical), other), other)
end
assert(equality.valuesEqual(content(canonical .. "&version=2"), "https://www.roblox.com/asset/?id=123&version=2"))
assert(not equality.valuesEqual(canonical, "http://www.roblox.com/asset/?id=123"), "ordinary strings keep their spelling")
assert(not equality.valuesEqual({ SourceType = kinds.Object }, ""), "object content is not empty URI content")

local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.luau")
local first = assert(source:find("local valuesEqual = BridgeValueEquality.valuesEqual", 1, true))
local last = assert(source:find("BridgeEditorSync.decodeValue =", first, true))
local propertyEqual = luau.load(source:sub(first, last - 1) .. "\nreturn propertyValuesEqual", {
	environment = {
		BridgeValueEquality = equality,
		RbxDomModule = { findCanonicalPropertyDescriptor = function(_, name)
			return { dataType = if name == "SoundId" then "ContentId" elseif name == "TextureContent" then "Content" else "String" }
		end },
	},
})()
local function instance(class)
	return { ClassName = class, IsA = function(_, name) return name == class end }
end
for _, name in { "SoundId", "TextureContent" } do
	assert(propertyEqual(instance("Sound"), name, canonical, "http://www.roblox.com/asset/?id=123"), name)
	assert(not propertyEqual(instance("Sound"), name, canonical, "rbxassetid://124"), name)
end
assert(not propertyEqual(instance("StringValue"), "Value", canonical, "http://www.roblox.com/asset/?id=123"))
assert(propertyEqual(instance("Lighting"), "TimeOfDay", "13:24:40", "-10:35:20"))
assert(propertyEqual(instance("Lighting"), "TimeOfDay", "01:00:00", "25:00"))
assert(not propertyEqual(instance("Lighting"), "TimeOfDay", "13:24:40", "13:24:41"))
for _, name in { "FrontAccessory", "BackAccessory", "HairAccessory" } do
	assert(propertyEqual(instance("HumanoidDescription"), name, "123,456", "456, 123"), name)
	assert(not propertyEqual(instance("HumanoidDescription"), name, "123,456", "123,789"), name)
	assert(not propertyEqual(instance("HumanoidDescription"), name, "123,456", "123,123,456"), name)
end
print("Value rewrites: asset types, authored strings, clock values and accessory lists")
