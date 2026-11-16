local fs = require("@lune/fs")
local luau = require("@lune/luau")

-- A store pulled in an earlier Studio session names instances by ids this
-- session does not have. Each such lookup used to read the id of every
-- instance of the service again; a review that decodes 1,700 references in a
-- 20,000-instance service spent twenty seconds on it. One pass per state
-- answers every miss after it.
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.luau")
local first = assert(source:find("local function resolveInstanceBySettingsId(", 1, true))
local after = assert(source:find("local function instanceMatchesExpectedClass(", first, true))

local idReads = 0
local function instance(id)
	return {
		GetDebugId = function()
			idReads += 1
			return id
		end,
	}
end
local state = { instances = {} }
for index = 1, 500 do
	state.instances[index] = instance("1_" .. index)
end
local cache = { lookup = {}, state = state }
local ctx = { settingsIdLookupByService = { Workspace = cache }, identityModule = {} }

local resolve = luau.load(source:sub(first, after - 1) .. "\nreturn resolveInstanceBySettingsId", {
	environment = {
		settingsIdText = function(raw)
			return if type(raw) == "string" then raw else nil
		end,
		strongSettingsId = function(raw)
			return type(raw) == "string" and string.sub(raw, 1, 6) == "debug:"
		end,
		settingsIdLookupForService = function()
			return cache.lookup, cache.state
		end,
		getStateForService = function()
			return state
		end,
		liveInstance = function(value)
			return value
		end,
		parseInstanceIndexId = function()
			return nil
		end,
	},
})()

for miss = 1, 200 do
	assert(resolve("Workspace", "debug:9_" .. miss, ctx) == nil, "an id of another session resolved")
end
assert(idReads <= #state.instances, `200 misses read {idReads} ids of {#state.instances} instances`)
assert(resolve("Workspace", "debug:1_77", ctx) == state.instances[77], "an id of this session was not found")

-- An instance the state gained since is found, at the cost of one more pass.
idReads = 0
state.instances[501] = instance("1_501")
assert(resolve("Workspace", "debug:1_501", ctx) == state.instances[501], "a new instance was not found")
assert(resolve("Workspace", "debug:9_1", ctx) == nil)
assert(idReads <= #state.instances + 1, `the second pass read {idReads} ids`)

print("settings id scan workloads passed")
