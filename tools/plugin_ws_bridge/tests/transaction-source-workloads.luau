local fs = require("@lune/fs")
local luau = require("@lune/luau")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.lua")
local function section(first, after)
	local start = assert(source:find(first, 1, true))
	return source:sub(start, assert(source:find(after, start, true)) - 1)
end

local service = {}
local function scriptValue(value, parent)
	return {
		ClassName = "LocalScript", Source = value, Parent = parent,
		IsDescendantOf = function(self, root) return self.Parent == root end,
		GetFullName = function() return "LoadingScreen" end,
	}
end
local live = scriptValue("old live source", service)
local staged = scriptValue("new staged source", nil)
local currentLease, cancelled = "owner", false
local transaction = {
	state = "prepared", leaseId = "owner",
	studioGenerations = { ReplicatedFirst = 0 },
	nativeImportServices = { ReplicatedFirst = true },
	resolveStagedPath = function() return staged end,
}
local api = {}
local guard = luau.load(
	section("local function validateChangePath(", "local function validatedChangeService(")
		.. section("local function assertInstanceInService(", "local function isProtectedWorkspaceCameraInstance(")
		.. section("\tlocal function assertTransactionLease(", "\tlocal function recordTransactionOutcome(")
		.. section("\tfunction api.getLiveSourceBatch(", "\tlocal function captureOperationCancellation(")
		.. "\nreturn assertChangeInstanceInService",
	{ environment = {
		api = api, editorTransactions = { active = transaction },
		ctx = { maxChangesPerRequest = 5000, luaSourceClass = { LocalScript = true } },
		currentRequestLeaseId = function() return currentLease end,
		assertRequestLeaseActive = function() assert(not cancelled, "cancelled") end,
		denseArrayLength = function(values) return type(values) == "table", #values end,
		resolvePathSegments = function() return live end,
		readScriptSource = function(instance) return true, instance.Source end,
		pathKey = function(segments) return table.concat(segments, ".") end,
	} }
)()
local selector = { index = 7, pathSegments = { "ReplicatedFirst", "LoadingScreen" }, pathOrdinals = { 1, 2 } }
local request = { selectors = { selector }, transactionId = "active" }
assert(api.getLiveSourceBatch({ selectors = { selector } }).rows[1].source == live.Source)
local row = api.getLiveSourceBatch(request).rows[1]
assert(row.index == 7 and row.source == staged.Source)
guard(staged, service, selector, transaction)
assert(not pcall(guard, scriptValue("foreign", nil), service, selector, transaction))
guard(live, service, selector, {})
staged = nil
assert(api.getLiveSourceBatch(request).rows[1].error == "Script was not found", "must not verify old live contents")
transaction.nativeImportServices.ReplicatedFirst = nil
assert(api.getLiveSourceBatch(request).rows[1].source == live.Source, "existing additive-import siblings remain live")
staged = scriptValue("additive source", nil)
assert(api.getLiveSourceBatch(request).rows[1].source == staged.Source, "new additive roots verify staged contents")
transaction.nativeImportServices.ReplicatedFirst = true
selector.pathSegments[1] = "ServerStorage"
assert(not pcall(api.getLiveSourceBatch, request))
selector.pathSegments[1] = "ReplicatedFirst"
request.transactionId = "missing"
assert(not pcall(api.getLiveSourceBatch, request))
request.transactionId = "active"
transaction.state = "committed"
assert(not pcall(api.getLiveSourceBatch, request))
transaction.state = "prepared"
currentLease = "foreign"
assert(not pcall(api.getLiveSourceBatch, request))
currentLease, cancelled = "owner", true
assert(not pcall(api.getLiveSourceBatch, request))
print("Transaction source verification: staged contents, ownership, cancellation and service boundaries preserved")

local function pathKey(segments, ordinals)
	return table.concat(segments, "/") .. ":" .. table.concat(ordinals, ",")
end
local overlay = require("../BridgeReferenceOverlay.module").create({
	pathCacheKey = pathKey,
	resolveOrdinalChild = function(parent, name) return parent[name] end,
	resolvePathSegments = function() return live end,
})
local path = { "StarterPlayer", "StarterCharacterScripts", "Script" }
local ordinals = { 1, 1, 2 }
local wanted = {}
local broad = {
	targetPath = { "StarterPlayer" },
	incomingRootsByPath = { [pathKey({ "StarterPlayer", "StarterCharacterScripts" }, { 1, 1 })] = { Script = live } },
}
local specific = {
	targetPath = { "StarterPlayer", "StarterCharacterScripts" },
	incomingRootsByPath = { [pathKey(path, ordinals)] = wanted },
}
for _, groups in { { broad, specific }, { specific, broad } } do
	assert(overlay.resolvePreparedPath(groups, path, ordinals, {}) == wanted)
	assert(overlay.resolvePreparedPath(groups, path, { 1, 1, 1 }, {}) == nil)
end
print("Nested import resolution: most-specific group wins without stale or duplicate-ordinal fallback")

broad.additive = true
local groups = { broad }
assert(overlay.resolvePreparedPath(groups, { "StarterPlayer", "Existing" }, { 1, 1 }, {}) == live)
assert(overlay.resolvePreparedPath(groups, { "StarterPlayer", "StarterCharacterScripts", "Missing" }, { 1, 1, 1 }, {}) == nil)
assert(overlay.resolvePreparedPath(groups, { "StarterPlayer", "StarterCharacterScripts", "Script" }, { 1, 1, 1 }, {}) == live)
print("Additive import resolution: existing siblings remain accessible without missing-descendant fallback")
