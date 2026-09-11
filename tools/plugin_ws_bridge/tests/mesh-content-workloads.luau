local fs = require("@lune/fs")
local luau = require("@lune/luau")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.lua")
do
	local begin = assert(source:find("local runtimePropertiesByClass =", 1, true))
	local finish = assert(source:find("local function decodePropertyValue(", begin, true))
	local has = luau.load(source:sub(begin, finish - 1) .. "\nreturn classHasProperty", { environment = {
		RbxDomModule = { findCanonicalPropertyDescriptor = function(_, name) return if name == "Known" then {} else nil end },
		typeof = function(value) return if type(value) == "table" and value.kind then value.kind else typeof(value) end,
	} })()
	local reads = 0
	local instance = setmetatable({ ClassName = "FutureClass", IsA = function() return false end }, { __index = function(_, name)
		reads += 1
		if name == "FutureFlag" then return false end
		if name == "FutureRef" then return nil end
		if name == "Child" then return { kind = "Instance" } end
		if name == "Method" then return function() end end
		if name == "Signal" then return { kind = "RBXScriptSignal" } end
		error("not a readable member")
	end })
	assert(has(instance, "Known") and reads == 0)
	for _ = 1, 100 do assert(has(instance, "FutureFlag")) end
	assert(reads == 1, "new scalar property detection must be cached")
	assert(has(instance, "FutureRef"))
	assert(has(instance, "Child") and has(instance, "Child") and reads == 4, "child lookup must not poison class cache")
	assert(not has(instance, "Method") and not has(instance, "Signal") and not has(instance, "Unknown"))
end
local first = assert(source:find("local function changedPropertyNames(", 1, true))
local last = assert(source:find("local function isMeshGeometryProperty(", first, true))
local retained, assigned = "old", 0
local functions = luau.load(source:sub(first, last - 1) .. "\nreturn { changedPropertyNames, writeDecodedProperty }", { environment = {
	readProperty = function() return true, retained end,
	valuesEqual = function(a, b) return a == b end,
	writePropertyForSync = function(_, _, value) assigned += 1; retained = value; return true end,
} })()
local names, write = functions[1], functions[2]
local order = names({ MeshContent = "modern", MeshId = "legacy", MeshSize = {}, CollisionFidelity = "Hull" })
assert(order[1] == "MeshId" and order[2] == "MeshContent" and order[3] == "MeshSize" and #order == 4)
local objectContent = {}
assert(write({}, "MeshContent", objectContent, {}, { propertyUpdated = 0 }))
assert(assigned == 1 and retained == objectContent, "object-backed Content must not become a URI")

do
	local writes = {}
	local apply = luau.load(source:sub(first, last - 1) .. "\nreturn writeDecodedProperty", { environment = {
		readProperty = function(instance, name) return true, instance[name] end,
		valuesEqual = function(a, b) return a == b end,
		writePropertyForSync = function(instance, name, value)
			writes[#writes + 1] = name
			instance[name] = value
			if name == "Texture" then instance.TextureContent = value end
			return true
		end,
	} })()
	local texture = { Texture = "old", TextureContent = "old" }
	local stats = { noops = 0, propertyUpdated = 0 }
	assert(apply(texture, "Texture", "new", {}, stats))
	assert(apply(texture, "TextureContent", "new", {}, stats))
	assert(#writes == 1 and stats.noops == 1, "An auto-populated value must not be written twice")
	assert(apply(texture, "TextureContent", objectContent, {}, stats))
	assert(#writes == 2 and texture.TextureContent == objectContent,
		"An auto-populated value must not replace a different explicitly requested value")
end

assert(not source:find("CreateMeshPartAsync", 1, true), "forward writes must not download donor geometry")
assert(not source:find("preloadPropertyMeshPartSources", 1, true), "forward writes must not scan for donor meshes")
print("Mesh content: dependency ordering, Object Content preserved, no donor-loading path")

do
	local enumerationCount = 0
	local enum = { None = { Value = 0 }, Activate = { Value = 1 }, All = { Value = 100 } }
	enum.GetEnumItems = function()
		enumerationCount += 1
		return { enum.None, enum.Activate, enum.All }
	end
	local codec = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeValueCodec.module.lua"), { environment = {
		game = { GetService = function() return {} end }, Enum = { InputSink = enum },
	} })()
	for _ = 1, 100 do
		for _, name in { "0", "1", "100", "None", "Activate", "All" } do
			local ok, item = codec.decode({ _type = "EnumItem", name = name }, "InputSink")
			assert(ok and item == (enum[name] or ({ enum.None, enum.Activate, [101] = enum.All })[tonumber(name) + 1]))
		end
	end
	assert(enumerationCount == 1, "numeric enum decoding must reuse its enum lookup")
	for _, name in { "99", "-1", "1.5", "Missing" } do
		assert(not codec.decode({ _type = "EnumItem", name = name }, "InputSink"))
	end
	assert(not codec.decode({ _type = "EnumItem", name = "0", enumType = "Foreign" }, "InputSink"))
end

first = assert(source:find("local function restoreMeshGeometry(", 1, true))
last = assert(source:find("local function setTagForSync(", first, true))
for _, failure in { false, true } do
	local expected, cancelled = {}, 0
	local target = { Size = "original size", TextureContent = "original texture" }
	target.IsDescendantOf = function() return true end
	target.ApplyMesh = function()
		assert(expected.MeshSize == "saved size", "rollback must expect the coupled MeshSize event")
		if failure then error("rollback failed") end
	end
	local restore = luau.load(source:sub(first, last - 1) .. "\nreturn restoreMeshGeometry", { environment = {
		game = {},
		cancelExpectedEvent = function() cancelled += 1 end,
		writePropertyForSync = function(instance, name, value) instance[name] = value; return true end,
	} })()
	local ok = restore(target, { MeshSize = "saved size" }, {
		expectPropertyEvent = function(_, name, value) expected[name] = value; return {} end,
	})
	assert(ok ~= failure and cancelled == (if failure then 9 else 0))
	assert(target.Size == "original size" and target.TextureContent == "original texture")
end
