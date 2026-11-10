local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")
local Overlay = require("../BridgeReferenceOverlay.module")
local Retarget = require("../BridgeReferenceRetarget.module")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.luau")
local runtimeSource = fs.readFile("tools/plugin_ws_bridge/BridgePluginRuntime.module.luau")

local function slice(text, first, after, from)
	local start = assert(text:find(first, from or 1, true))
	local finish = assert(text:find(after, start, true))
	return text:sub(start, finish - 1), start
end

local function folder(name, parent)
	local instance = roblox.Instance.new("Folder")
	instance.Name, instance.Parent = name, parent
	return instance
end

local place = roblox.Instance.new("Folder")
local replicated = folder("ReplicatedStorage", place)
local server = folder("ServerStorage", place)
local services = { ReplicatedStorage = replicated, ServerStorage = server }

-- The shipped identity registry, the shipped preparation of a deserialized
-- import and the shipped rows a capture carries.
local identity = luau.load(slice(source, "local function settingsIdText(", "local function getStateForService(")
	.. "\nreturn { rememberRoots = rememberImportedRootIdentities, captured = capturedSettingsId }", { environment = {
	BridgeIdentity = { liveInstance = function(value)
		return if value ~= nil and value.Parent ~= nil and value:IsDescendantOf(place) then value else nil
	end },
	game = { GetService = function(_, name) return services[name] end },
} })()
local _, finishStart = slice(source, "\tfunction api.finishBinaryImport(", "\tfunction api.cancelBinaryImport(")
local prepareImport = luau.load(
	"return function(session, wrappedRootsByName, outgoingByGroup, ctx)\nlocal prepared, detachedRoots, operation = {}, {}, nil\n"
		.. slice(source, "\t\t\tlocal payloadVerifiedServices = {}",
			'\t\t\tsetTrackedOperationPhase(operation, "prepareRetention")', finishStart)
		.. "\nreturn prepared\nend",
	{ environment = {
		setTrackedOperationPhase = function() end,
		setParentForSync = function(instance, parent) instance.Parent = parent end,
	} }
)()
local function captureRows(ctx)
	return luau.load("local refreshMatchedSettingsIds, matchedSettingsIdsForRange\n"
		.. slice(runtimeSource, "\trefreshMatchedSettingsIds = function(", "\tlocal function tryReadModelPivotProperty(")
		.. "\nreturn matchedSettingsIdsForRange", { environment = { editorSync = {
		matchedSettingsId = function(instance) return identity.captured(instance, ctx) end,
		matchedSettingsIdVersion = function() return tonumber(ctx.matchedSettingsIdVersion) or 0 end,
	} } })()
end

local overlay = Overlay.create({
	RbxDomModule = { getReferencePropertyNames = function() return {} end, getObjectContentPropertyNames = function() return {} end },
	BridgeReferenceRetarget = Retarget,
	pathCacheKey = function(segments, ordinals) return table.concat(segments, "/") .. table.concat(ordinals, ",") end,
	captureExplorerSelection = function() return {} end, restoreExplorerSelection = function() end,
	setParentForSync = function(value, parent) value.Parent = parent end,
	removeInstanceForUndo = function() error("additive imports must not remove live roots") end,
})

-- `ba SERVICE -n NAME -c Folder --push` as Studio receives it without the
-- native reader: one new root, deserialized, staged and attached at commit.
local function pushNewRoot(name, rootSettingsIds, ctx)
	local payload = roblox.Instance.new("Folder")
	folder("__ReniumImportRoot_1", payload)
	local group = {
		serviceName = "ReplicatedStorage", service = replicated, target = replicated,
		targetPath = { "ReplicatedStorage" }, additive = true, count = 1, payloadRootName = "payload",
		rootPaths = { { pathSegments = { "ReplicatedStorage", name }, pathOrdinals = { 1, 1 } } },
		rootSettingsIds = rootSettingsIds, retainedRoots = {}, packageRoots = {}, changeGeneration = 0,
	}
	local prepared = prepareImport({ groups = { group } }, { payload = payload }, { {} }, ctx)
	local root = prepared[1].incomingByPayloadIndex[1]
	identity.rememberRoots(prepared, ctx)
	assert(identity.captured(root, ctx) == nil, "A staged root is not in the place and has no identity yet")
	local undo = overlay.prepareRetained(prepared, ctx, true)
	undo.prepared, undo.generationsByService, undo.guard =
		prepared, { ReplicatedStorage = 0 }, { connections = {}, attributeCheckpoints = {} }
	overlay.commitNative(undo, ctx)
	assert(root.Parent == replicated and root.Name == name)
	identity.rememberRoots(prepared, ctx)
	return root
end
local function context()
	return { studioChangeGeneration = function() return 0 end, allowedServices = {}, invalidateService = function() end }
end

local ctx = context()
local rows = captureRows(ctx)
local target = pushNewRoot("RefTarget", { "editor:6c71" }, ctx)
assert(identity.captured(target, ctx) == "editor:6c71", "A pushed root keeps the store identity of its push")

-- Studio renames the pushed folder and a new folder takes its name.
target.Name = "RefTargetMoved"
local newcomer = folder("RefTarget", replicated)
assert(identity.captured(target, ctx) == "editor:6c71" and identity.captured(newcomer, ctx) == nil)
local captured = rows({ instances = { replicated, target, newcomer } }, 1, 3)
assert(#captured == 1 and captured[1][1] == 2 and captured[1][2] == "editor:6c71",
	"The capture names the renamed instance by its store id and the newcomer by none")

-- Moved under another parent it is the same store instance; in another
-- service it is a new instance of that store, and back again the old one.
target.Parent = folder("Holder", replicated)
assert(identity.captured(target, ctx) == "editor:6c71")
target.Parent = server
assert(identity.captured(target, ctx) == nil, "A store id is not reported into another service's capture")
assert(rows({ instances = { server, target } }, 1, 2) == nil)
target.Parent = replicated
assert(identity.captured(target, ctx) == "editor:6c71")
target.Parent = nil
assert(identity.captured(target, ctx) == nil and rows({ instances = { replicated, newcomer } }, 1, 2) == nil)

-- A root pushed without an identity, and a Studio no push has reached.
local plainContext = context()
local plain = pushNewRoot("Plain", nil, plainContext)
assert(plainContext.matchedSettingsIdVersion == nil and identity.captured(plain, plainContext) == nil)
assert(captureRows(plainContext)({ instances = { replicated, newcomer, plain } }, 1, 3) == nil,
	"A capture without push knowledge carries no store ids")

place:Destroy()
print("Pushed identity: a deserialized root keeps its store id through a rename, a newcomer and a move; never in another service")
