local fs = require("@lune/fs")
local luau = require("@lune/luau")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.luau")
local function extract(first, after, environment, returned)
	local start = assert(source:find(first, 1, true))
	local finish = assert(source:find(after, start, true))
	return luau.load(source:sub(start, finish - 1) .. "\nreturn " .. returned, { environment = environment })()
end

local changedPropertyNames = extract("local function changedPropertyNames(", "local function writeDecodedProperty(", {
	pairs = pairs, tostring = tostring,
}, "changedPropertyNames")

local writes = {}
local function part()
	return {
		ClassName = "Part",
		IsA = function(_, class) return class == "BasePart" end,
		GetAttributes = function() return {} end,
		GetFullName = function() return "Workspace.Part" end,
	}
end
local instance
local applyPropertyChange = extract("local function applyPropertyChange(", "local function validateObjectTable(", {
	type = type, tostring = tostring, ipairs = ipairs, pairs = pairs, error = error, string = string,
	changedPropertyNames = changedPropertyNames,
	validatedChangeService = function() return "Workspace", {} end,
	resolveInstance = function() return instance end,
	assertChangeInstanceInService = function() end,
	isProtectedWorkspaceCameraInstance = function() return false end,
	applyChangedProperty = function(_, name, value) writes[#writes + 1] = name .. "=" .. tostring(value) end,
	writePropertyForSync = function(_, name, value)
		writes[#writes + 1] = name .. "=" .. tostring(value) .. " (new part's value)"
		return true
	end,
	resetProperty = function(_, name) writes[#writes + 1] = "reset " .. name end,
}, "applyPropertyChange")

local function apply(held, change)
	table.clear(writes)
	instance = part()
	local transaction = { heldAnchored = if held then { [instance] = true } else {} }
	applyPropertyChange(change, { editorTransaction = transaction }, {}, {})
	assert(next(transaction.heldAnchored) == nil, "A row releases the part it places")
	return table.concat(writes, ", ")
end

assert(apply(false, { properties = { Anchored = false, CFrame = "there" } }) == "Anchored=false, CFrame=there",
	"A part Studio already held is anchored or loosened before it is placed")
assert(apply(true, { properties = { Anchored = false, CFrame = "there" } }) == "CFrame=there, Anchored=false",
	"A part held anchored since its creation takes its own value once placed")
assert(apply(true, { properties = { Anchored = true, CFrame = "there" } }) == "CFrame=there, Anchored=true")
assert(apply(true, { properties = { CFrame = "there" } }) == "CFrame=there, Anchored=false (new part's value)",
	"A held part whose row states no value is a new part's again")
assert(apply(true, { resetProperties = { "Anchored" } }) == "Anchored=false (new part's value), reset Anchored")
assert(apply(true, {}) == "Anchored=false (new part's value)")

local created = {}
local syncDesiredEntry = extract("local function syncDesiredEntry(", "local function removeUnknownInstances(", {
	pcall = pcall, tostring = tostring, error = error,
	Instance = { new = function(className)
		local made = { ClassName = className, IsA = function(self, class)
			return class == "BasePart" and self.ClassName == "Part"
		end }
		created[#created + 1] = made
		return made
	end },
	liveInstance = function() return nil end,
	resolveEntryInstance = function() return nil end,
	isProtectedWorkspaceCameraInstance = function() return false end,
	resolveEntryParent = function() return {} end,
	setParentForSync = function(made) made.anchoredWhenParented = made.Anchored end,
	rememberEntryResolution = function() end,
}, "syncDesiredEntry")
local transaction = { createdInstances = {}, heldAnchored = {} }
for _, className in { "Part", "Folder" } do
	local entry = { key = className, className = className, pathSegments = { "Workspace", className }, previousPathSegments = {} }
	syncDesiredEntry(entry, "Workspace", { editorTransaction = transaction }, { instanceCreated = 0, noops = 0 }, {}, {}, true)
end
assert(created[1].anchoredWhenParented == true and transaction.heldAnchored[created[1]] == true,
	"A new part enters Studio anchored and is remembered")
assert(created[2].Anchored == nil and transaction.heldAnchored[created[2]] == nil, "Only parts are held anchored")
local outside = { createdInstances = {} }
syncDesiredEntry({ key = "Part", className = "Part", pathSegments = { "Workspace", "Part" }, previousPathSegments = {} },
	"Workspace", { editorTransaction = outside }, { instanceCreated = 0, noops = 0 }, {}, {}, true)
assert(created[3].Anchored == nil, "A transaction that releases no part holds none")
print("new parts stay anchored until their rows place them")
