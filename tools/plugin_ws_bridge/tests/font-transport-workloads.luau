local fs = require("@lune/fs")
local luau = require("@lune/luau")

local normal, italic = { Name = "Normal" }, { Name = "Italic" }
local weight = { Name = "Medium" }
local function font(style, cache)
	return { Family = "family", Weight = weight, Style = style, cache = cache, kind = "Font" }
end
local current = font(normal, "old")
local desired = font(normal, "new")
local function equal(a, b)
	return a.Family == b.Family and a.Weight == b.Weight and a.Style == b.Style
end
local function extract(first, after, environment, returned)
	local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.lua")
	local start = assert(source:find(first, 1, true))
	local finish = assert(source:find(after, start, true))
	return luau.load(source:sub(start, finish - 1) .. "\nreturn " .. returned, { environment = environment })()
end
local writes, tokens = 0, 0
local instance = { IsA = function() return false end }
local write = extract("local function writePropertyForSync(", "local function setAttributeForSync(", {
	typeof = function(value) return if type(value) == "table" then value.kind else typeof(value) end,
	Font = { new = function(_, _, style) return font(style) end },
	Enum = { FontStyle = { Normal = normal, Italic = italic } },
	readProperty = function() return true, current end,
	valuesEqual = equal,
	writeProperty = function(_, _, value)
		writes += 1
		if not equal(current, value) then current = value end
		return true
	end,
	markLiveMutation = function() end,
}, "writePropertyForSync")
local ctx = { expectPropertyEvent = function() tokens += 1 end }
for _ = 1, 100 do
	local before = current
	assert(write(instance, "FontFace", desired, ctx))
	assert(current.cache == "new", "same public fields must still replace cached face")
	assert(write(instance, "FontFace", before, ctx))
	assert(current.cache == "old", "rollback must restore the original cached face")
end
assert(writes == 400 and tokens == writes, "both synchronous writes need exact event expectations")

local destroyed = 0
local roots
local environment = {
	game = { GetService = function(_, name)
		if name == "EncodingService" then return { Base64Decode = function(_, value) return value end } end
		return { DeserializeInstancesAsync = function() return roots end }
	end },
}
local codec = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeValueCodec.module.lua"), { environment = environment })()
local raw = { _type = "Font", family = "family", weight = "Medium", style = "Normal", _nativeFont = "payload" }
local carrier = { ClassName = "TextLabel", FontFace = desired, GetChildren = function() return {} end,
	Destroy = function() destroyed += 1 end }
roots = { carrier }
local ok, decoded = codec.decode(raw)
assert(ok and decoded == desired and destroyed == 1)
for _, invalid in { {}, { carrier, carrier }, { { ClassName = "Script", Destroy = carrier.Destroy } } } do
	roots = invalid
	assert(not codec.decode(raw), "wrong native payload shape must be rejected")
end
roots = { carrier }
raw.family = "other"
assert(not codec.decode(raw), "payload fields must match the requested Font")
raw._nativeFont = string.rep("x", 16385)
assert(not codec.decode(raw), "payload size must be bounded before deserialization")
print("Font transport: exact cache replacement and rollback, expected events, bounded native payload validation")

do
	local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.lua")
	local start = assert(source:find("\tlocal nativeFont = type(rawValue)", 1, true))
	local finish = assert(source:find("\tif okWrite then", start, true))
	local block = "return function(instance, propertyName, rawValue, ctx, stats, serviceName)\n"
		.. source:sub(start, finish - 1) .. "\nreturn okWrite end"
	for _, fault in { "none", "cancel", "move", "rename" } do
		local target = { Parent = {}, Name = "Original" }
		local cancelled, mutated = false, false
		local apply = luau.load(block, { environment = {
			decodePropertyValue = function()
				if fault == "cancel" then cancelled = true end
				if fault == "move" then target.Parent = {} end
				if fault == "rename" then target.Name = "Changed" end
				return true, desired
			end,
			writeDecodedProperty = function() mutated = true; return true end,
		} })()
		local ok = pcall(apply, target, "FontFace", { _nativeFont = "payload" }, {
			assertEditorMutationActive = function() assert(not cancelled, "cancelled") end,
		}, {}, "TextChatService")
		assert(ok == (fault == "none") and mutated == ok, `no write after native decode {fault}`)
	end
end
