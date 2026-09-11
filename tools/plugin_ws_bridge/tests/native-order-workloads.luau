local fs = require("@lune/fs")
local luau = require("@lune/luau")

-- Exercise both shipped invalidation and cache reuse. A within-service move
-- emits Parent without a service DescendantAdded/Removing event.
local source = fs.readFile("tools/plugin_ws_bridge/BridgePluginRuntime.module.lua")
local first = assert(source:find("local nativeStructureGenerationByService:", 1, true))
local last = assert(source:find("function Config.updateStatusText()", first, true))
local prepareFirst = assert(source:find("prepareNativeState = function(", last, true)) + #"prepareNativeState = "
local prepareLast = assert(source:find("\n\t\tincludeExportInstance =", prepareFirst, true))
local prepareSource = source:sub(prepareFirst, prepareLast - 1):gsub(",%s*$", "")
local service = { ClassName = "ServerStorage" }
local left = { ClassName = "Folder", Parent = service, Archivable = true }
local right = { ClassName = "Folder", Parent = service, Archivable = true }
local target = { ClassName = "NumberValue", Parent = left, Archivable = true }
local ordered = { service, left, target, right }
local indexed, onProperty
local states = {}
local config = { syncProfile = {}, studioChanges = {
	observeExports = function(_, property) onProperty = property end,
	trackedServiceRoot = function() return service end,
	isTracking = function() return true end,
	hasNonArchivable = function() return false end,
	invalidateExportCache = function() indexed = nil end,
	exportInstances = function()
		indexed = indexed or table.clone(ordered)
		return indexed
	end,
} }
local prepare = luau.load(source:sub(first, last - 1) .. "\nreturn " .. prepareSource, { environment = {
	Config = config,
	PropertySchemaModule = { makeExportPropertyFilter = function() return function() return true end end },
	ALLOWED_SERVICES = { ServerStorage = true },
	game = { GetService = function() return service end },
	typeof = function() return "Instance" end,
	includeExportInstance = function() return true end,
	nativeStateByService = states,
	nativeExportGeneration = function(name) return config.exportProofGeneration(name) end,
	refreshMatchedSettingsIds = function() return false end,
	prepareService = function(_, _, _, _, instances)
		return nil, { instances = instances, batchCacheByKey = {}, batchCacheKeys = {},
			sourceBatchCacheByKey = {}, sourceBatchCacheKeys = {}, scriptObjects = {}, scriptSourcesByIndex = {} }
	end,
} })()
local cached = prepare("ServerStorage")
for index = 1, 100 do
	onProperty(target, "Value")
	assert(prepare("ServerStorage") == cached, "property edits must reuse structure")
	target.Parent = if index % 2 == 1 then right else left
	ordered = if index % 2 == 1 then { service, left, right, target } else { service, left, target, right }
	onProperty(target, "Parent")
	local updated = prepare("ServerStorage")
	assert(updated ~= cached, "within-service move must rebuild native identities")
	for position, instance in ipairs(ordered) do
		assert(updated.instances[position] == instance, "native overlay must use current serializer order")
	end
	assert(prepare("ServerStorage") == updated, "unchanged state must remain cached")
	cached = updated
end
print("Native export order: 100 within-service moves preserve identity ordering and unchanged cache reuse")
