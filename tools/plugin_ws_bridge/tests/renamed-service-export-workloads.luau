local fs = require("@lune/fs")
local luau = require("@lune/luau")
local source = fs.readFile("tools/plugin_ws_bridge/BridgePluginRuntime.module.luau")
local game = {}
local service = { ClassName = "ServerStorage", Name = "Vault", Parent = game, Archivable = true }
local child = { ClassName = "Part", Name = "Pushed", Parent = service, Archivable = true }
function game:GetService(name) assert(name == "ServerStorage"); return service end
function game:FindService(name) return if name == "ServerStorage" then service else nil end
function service:GetChildren() return { child } end
function service:GetDescendants() return { child } end
function service:IsDescendantOf(root) return root == game end
function child:IsDescendantOf(root) return root == game or root == service end
function service:GetDebugId() return "fixture-service" end
function child:GetDebugId() return "fixture-child" end
local Identity = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeIdentity.module.luau"), {
	environment = { game = game },
})()
local environment = {
	game = game, IdentityModule = Identity, ALLOWED_SERVICES = { ServerStorage = true },
	ParallelModule = { newWorkBudget = function() return function() end end },
	Config = {
		LUA_SOURCE_CLASS = {},
		studioChanges = { serviceGeneration = function() return 0 end, hasNonArchivable = function() return false end },
		writeNativeOverlayDebugId = function(instance, data, index) data[index] = instance:GetDebugId() end,
	},
	excludedExportRoots = function() return {} end,
	isNeverSerialized = function() return false end,
	refreshMatchedSettingsIds = function() end,
	getServicePropertySchema = function() return {} end,
	getServiceEnumValueNamesByType = function() return {} end,
	stateByService = {},
}
local function extract(first, last, result)
	local start = assert(source:find(first, 1, true))
	local finish = assert(source:find(last, start, true))
	return luau.load("local " .. source:sub(start, finish - 1) .. "\nreturn " .. result, {
		environment = environment,
	})()
end
-- Run the shipped functions; only unrelated export dependencies are stubbed.
local prepare = extract("prepareService = function(", "\n\tfunction Config.getBridgeInfo", "prepareService")
local encodeRef = extract("function encodeComparableRefValue(", "\n\tencodeSchemaComparableValue =", "encodeComparableRefValue")
local buildRow = extract("function buildCompactV5Row(", "\n\tfunction Config.buildCompactV5Exporter", "buildCompactV5Row")
for _, native in { false, true } do
	local _, state = prepare("ServerStorage", native)
	assert(state.instances[1] == service and state.instances[2] == child)
	assert(Identity.getCachedInstancePath(state, child) == "ServerStorage.Pushed", "Export path seed must use the service class")
	local segments, ordinals = Identity.getCachedRefPathParts(state, child)
	assert(#segments == 2 and segments[1] == "ServerStorage" and segments[2] == "Pushed", "Export reference seed must use the service class")
	assert(#ordinals == 2 and ordinals[1] == 1 and ordinals[2] == 1)
	local strings = {}
	local row = buildRow(state, service, 1, 0, nil, nil, nil, nil, false, strings, {}, function(list, _, text)
		table.insert(list, text)
		return #list
	end)
	assert(strings[row[1]] == "ServerStorage", "The full export row must name its root by service class")
	if native then
		local ref = Identity.serializeRefValue(state, child)
		assert(ref.pathSegments[1] == "ServerStorage" and ref.pathSegments[2] == "Pushed")
		local compact = encodeRef(state, child)
		assert(compact[1] == 0 and compact[4] == "ServerStorage" and compact[5] == "Pushed", "Native export references must keep the canonical root")
	else
		assert(encodeRef(state, child) == 2, "Ordinary local references keep their instance index")
	end
	assert(service.Name == "Vault", "Exporting must not rename the live service")
end
print("Renamed service export: ordinary and native state keep canonical root paths and names")
