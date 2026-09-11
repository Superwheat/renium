local fs = require("@lune/fs")
local luau = require("@lune/luau")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeStudioChanges.module.lua")
local function section(first, after)
	local start = assert(source:find(first, 1, true))
	return source:sub(start, assert(source:find(after, start, true)) - 1)
end
local initialise = luau.load(section("\tlocal state: State = {", "\n\tlocal api = {}") .. "\nreturn state", {
	environment = { Instance = { new = function() return {} end } },
})
local state = initialise()
local api = {}
local consume, expect, clear = luau.load(
	section("\tlocal function consumeExpectedInstanceEvent(", "\tfunction api.expectParentChange(")
		.. section("\tfunction api.cancelExpectedEvent(", "\tfunction api.serviceGeneration(")
		.. section("\tlocal function clearExpectedEvents(", "\tfunction api.beginSuppress(")
		.. "\nreturn consumeExpectedInstanceEvent, expectInstanceEvent, clearExpectedEvents", {
		environment = { state = state, api = api, isSuppressed = function() return true end },
	}
)()
local function collect()
	collectgarbage("collect")
end
local function size(value)
	local count = 0
	for _ in value do count += 1 end
	return count
end
local witnesses = setmetatable({}, { __mode = "k" })
local tokens = {}
local maps = { state.expectedStructuralByInstance, state.expectedInstanceProperties,
	state.expectedInstanceAttributes, state.expectedTags }
local function createExpectations()
	for _, map in maps do
		for _ = 1, 1024 do
			local target = newproxy()
			witnesses[target] = map
			tokens[#tokens + 1] = expect(map, target, "event", "same", false, nil)
		end
	end
	for _ = 1, 1024 do
		local target = newproxy()
		witnesses[target] = state.expectedFreshInstances
		state.expectedFreshInstances[target] = true
	end
end
createExpectations()
collect()
assert(size(witnesses) == 5120, "GC discarded in-flight expectation targets")
local function consumeAll()
	local consumed = 0
	for target, map in witnesses do
		if map ~= state.expectedFreshInstances then
			local matched, token = consume(map, target, "event", "same", nil)
			assert(matched and not token.active)
			assert(map[target] == nil, "consumed expectation still owns its target")
			consumed += 1
		end
	end
	return consumed
end
assert(consumeAll() == 4096)
collect()
assert(size(witnesses) == 1024, `consumed event tokens retained targets: {size(witnesses)}`)
clear()
collect()
assert(size(witnesses) == 0, "settle cleanup leaked fresh-instance targets")
for _, map in maps do
	local target = newproxy()
	local token = expect(map, target, "event", nil, false, nil)
	api.cancelExpectedEvent(token)
	assert(not consume(map, target, "event", nil, nil), "cancelled expectation swallowed an edit")
	assert(map[target] == nil)
end
for _, key in { "propertyFingerprintByInstance", "propertyBaselineByInstance", "connectionServiceByInstance" } do
	assert(getmetatable(state[key]).__mode == "k", "ordinary observation caches must remain weak")
end
print("Expectation lifetime: GC retention, exact consumption, cancellation and settle release passed")

-- A transaction may temporarily retain objects under an unsynchronized service.
-- Completing the journal must release those connections even inside DataModel.
do
	local model, service = {}, {}
	local restored = { IsDescendantOf = function(_, parent) return parent == model or parent == service end }
	local retained = { IsDescendantOf = function(_, parent) return parent == model end }
	local released, disconnected = {}, {}
	local release = luau.load(section("\treleaseJournalObservers = function(", "\tlocal function connectInstance(")
		.. "\nreturn releaseJournalObservers", { environment = {
		game = model,
		state = { connectionServiceByInstance = { [restored] = "Workspace", [retained] = "Workspace" },
			serviceRoots = { Workspace = service } },
		releaseDetachedObserver = function(instance) released[instance] = true end,
		disconnectInstance = function(instance) disconnected[instance] = true end,
	} })()
	release({ detachedInstances = { [restored] = "Workspace", [retained] = "Workspace" } })
	assert(released[restored] and not disconnected[restored], "Restored instances must keep ordinary tracking")
	assert(disconnected[retained] and not released[retained], "Retained roots leaked their observers outside watched services")
end
