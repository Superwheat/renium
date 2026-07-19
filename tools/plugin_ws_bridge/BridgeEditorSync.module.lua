local BridgeEditorSync = {}

local BridgeCandidateMatch = require(script.Parent.BridgeCandidateMatch)
local BridgeInstanceSwap = require(script.Parent.BridgeInstanceSwap)
local BridgeReferenceRetarget = require(script.Parent.BridgeReferenceRetarget)
local BridgeValueEquality = require(script.Parent.BridgeValueEquality)
local BridgeValueCodec = require(script.Parent.BridgeValueCodec)
local ChangeHistoryService = game:GetService("ChangeHistoryService")
local CollectionService = game:GetService("CollectionService")
local EncodingService = game:GetService("EncodingService")
local InsertService = game:GetService("InsertService")
local Selection = game:GetService("Selection")
local SerializationService = game:GetService("SerializationService")
local Workspace = game:GetService("Workspace")
local ScriptEditorService = game:GetService("ScriptEditorService")

local RbxDomModule = require(script.Parent.RbxDom)

local function cloneArray(raw: any): { any }
	local out = {}
	if type(raw) ~= "table" then
		return out
	end
	for i, value in ipairs(raw) do
		out[i] = value
	end
	return out
end

local function denseArrayLength(raw: any): (boolean, number)
	if type(raw) ~= "table" then
		return false, 0
	end
	local length = #raw
	for key in pairs(raw) do
		if type(key) ~= "number" or key % 1 ~= 0 or key < 1 or key > length then
			return false, 0
		end
	end
	return true, length
end

local PATH_SEPARATOR = "\0"
local reconcileSessions = {}
local recentlyCreatedMeshParts = setmetatable({}, { __mode = "k" })
local binaryImports = {}
local completedBinaryImports = {}
local binaryExports = {}
local SESSION_TTL_SECONDS = 120
local MAX_RECONCILE_SESSIONS = 16
local MAX_RECONCILE_ENTRIES = 1000000
local MAX_BINARY_IMPORT_SESSIONS = 4
local MAX_BINARY_IMPORT_BUFFERED_BYTES = 536870912
local BINARY_IMPORT_CHUNK_BYTES = 524288
local COMPLETED_BINARY_IMPORT_TTL_SECONDS = 300
local MAX_COMPLETED_BINARY_IMPORTS = 64
local nextSessionExpiryToken = 0

local function countEntries(values: { [any]: any }): number
	local count = 0
	for _ in pairs(values) do
		count += 1
	end
	return count
end

local function pruneExpiredSessions(values: { [any]: any })
	local now = os.clock()
	for key, session in pairs(values) do
		if type(session) ~= "table" or now - (tonumber(session.updatedAt) or 0) > SESSION_TTL_SECONDS then
			values[key] = nil
			if type(session) == "table" and type(session.onExpire) == "function" then
				local okExpire, expireError = pcall(session.onExpire)
				if not okExpire then
					warn("[Renium] session expiry cleanup failed: " .. tostring(expireError))
				end
			end
		end
	end
end

local function pruneCompletedBinaryImports()
	local now = os.clock()
	local records = {}
	for importId, record in pairs(completedBinaryImports) do
		if type(record) ~= "table" or (tonumber(record.expiresAt) or 0) <= now then
			completedBinaryImports[importId] = nil
		else
			table.insert(records, { importId = importId, completedAt = tonumber(record.completedAt) or 0 })
		end
	end
	if #records <= MAX_COMPLETED_BINARY_IMPORTS then
		return
	end
	table.sort(records, function(a, b)
		return a.completedAt < b.completedAt
	end)
	for index = 1, #records - MAX_COMPLETED_BINARY_IMPORTS do
		completedBinaryImports[records[index].importId] = nil
	end
end

local function armSessionExpiry(values: { [any]: any }, key: any, session: { [any]: any })
	session.updatedAt = os.clock()
	if session.expiryArmed then
		return
	end
	nextSessionExpiryToken += 1
	local token = nextSessionExpiryToken
	session.expiryToken = token
	session.expiryArmed = true
	local function expireWhenIdle()
		local current = values[key]
		if type(current) ~= "table" or current.expiryToken ~= token then
			return
		end
		local idleSeconds = os.clock() - (tonumber(current.updatedAt) or 0)
		if idleSeconds > SESSION_TTL_SECONDS then
			values[key] = nil
			if type(current.onExpire) == "function" then
				local okExpire, expireError = pcall(current.onExpire)
				if not okExpire then
					warn("[Renium] session expiry cleanup failed: " .. tostring(expireError))
				end
			end
			return
		end
		task.delay(math.max(1, SESSION_TTL_SECONDS - idleSeconds + 1), expireWhenIdle)
	end
	task.delay(SESSION_TTL_SECONDS + 1, expireWhenIdle)
end

local function beginHistoryRecording(label: string): any?
	return ChangeHistoryService:TryBeginRecording(
		("Renium:%s:%s"):format(label, tostring(os.clock())),
		"Renium: " .. label
	)
end

local function finishHistoryRecording(recording: any?, operation: any?)
	if recording == nil then
		return
	end
	local finishOperation = operation or Enum.FinishRecordingOperation.Commit
	ChangeHistoryService:FinishRecording(recording, finishOperation)
end

local function captureExplorerSelection(): { Instance }
	local selected = Selection:Get()
	return if type(selected) == "table" then selected else {}
end

local function restoreExplorerSelection(selected: { Instance }, replacements: { [Instance]: Instance }?)
	local restored = {}
	for _, instance in ipairs(selected) do
		local candidate = if replacements ~= nil then replacements[instance] or instance else instance
		if typeof(candidate) == "Instance" and (candidate.Parent ~= nil or candidate:IsDescendantOf(game)) then
			restored[#restored + 1] = candidate
		end
	end
	Selection:Set(restored)
end

local function removeInstanceForUndo(instance: Instance)
	instance.Parent = nil
end

local function pathKey(pathSegments: any): string
	if type(pathSegments) ~= "table" then
		return ""
	end
	local out = table.create(#pathSegments)
	for i = 1, #pathSegments do
		out[i] = tostring(pathSegments[i])
	end
	return table.concat(out, PATH_SEPARATOR)
end

local function pathOrdinalsKey(pathOrdinals: any): string
	if type(pathOrdinals) ~= "table" then
		return ""
	end
	local out = table.create(#pathOrdinals)
	for i = 1, #pathOrdinals do
		out[i] = tostring(tonumber(pathOrdinals[i]) or 1)
	end
	return table.concat(out, ",")
end

local function pathCacheKey(pathSegments: any, pathOrdinals: any): string
	local base = pathKey(pathSegments)
	local ordinals = pathOrdinalsKey(pathOrdinals)
	if ordinals == "" then
		return base
	end
	return base .. PATH_SEPARATOR .. "ord" .. PATH_SEPARATOR .. ordinals
end

local function resolveOrdinalChild(parent: Instance, childName: string, ordinal: number): Instance?
	if ordinal <= 1 then
		return parent:FindFirstChild(childName)
	end
	local seen = 0
	for _, child in ipairs(parent:GetChildren()) do
		if child.Name == childName then
			seen += 1
			if seen == ordinal then
				return child
			end
		end
	end
	return nil
end

local function resolvePathSegments(pathSegments: any, resolveCache: { [string]: any }?, pathOrdinals: any?): Instance?
	if type(pathSegments) ~= "table" or #pathSegments == 0 then
		return nil
	end

	local cacheKey = nil
	if resolveCache ~= nil then
		cacheKey = pathCacheKey(pathSegments, pathOrdinals)
		local cached = resolveCache[cacheKey]
		if cached ~= nil then
			if cached == false then
				return nil
			end
			if typeof(cached) == "Instance" and cached.Parent ~= nil and cached:IsDescendantOf(game) then
				return cached
			end
		end
	end

	local first = tostring(pathSegments[1])
	local current = game:GetService(first)
	if current == nil then
		if resolveCache ~= nil and cacheKey ~= nil then
			resolveCache[cacheKey] = false
		end
		return nil
	end

	for i = 2, #pathSegments do
		local ordinal = if type(pathOrdinals) == "table" then tonumber(pathOrdinals[i]) or 1 else 1
		current = resolveOrdinalChild(current, tostring(pathSegments[i]), ordinal)
		if current == nil then
			if resolveCache ~= nil and cacheKey ~= nil then
				resolveCache[cacheKey] = false
			end
			return nil
		end
	end
	if resolveCache ~= nil and cacheKey ~= nil then
		resolveCache[cacheKey] = current
	end
	return current
end

local function settingsIdText(raw: any): string?
	if raw == nil then
		return nil
	end
	local text = string.match(tostring(raw), "^%s*(.-)%s*$") or ""
	if text == "" then
		return nil
	end
	return text
end

local function strongSettingsId(raw: any): boolean
	local settingsId = settingsIdText(raw)
	return settingsId ~= nil and string.sub(settingsId, 1, 6) == "debug:"
end

local function liveInstance(value: any): Instance?
	if typeof(value) == "Instance" and value.Parent ~= nil and value:IsDescendantOf(game) then
		return value
	end
	return nil
end

local function matchedSettingsInstance(serviceName: string, rawSettingsId: any, ctx: { [string]: any }): Instance?
	local settingsId = settingsIdText(rawSettingsId)
	if settingsId == nil or type(ctx.matchedSettingsInstancesByService) ~= "table" then
		return nil
	end
	local serviceMatches = ctx.matchedSettingsInstancesByService[serviceName]
	if type(serviceMatches) ~= "table" then
		return nil
	end
	local instance = liveInstance(serviceMatches[settingsId])
	if instance == nil then
		serviceMatches[settingsId] = nil
	end
	return instance
end

local function rememberMatchedSettingsInstance(
	serviceName: string,
	rawSettingsId: any,
	instance: Instance,
	ctx: { [string]: any }
)
	local settingsId = settingsIdText(rawSettingsId)
	if settingsId == nil or liveInstance(instance) == nil then
		return
	end
	if type(ctx.matchedSettingsInstancesByService) ~= "table" then
		ctx.matchedSettingsInstancesByService = {}
	end
	local serviceMatches = ctx.matchedSettingsInstancesByService[serviceName]
	if type(serviceMatches) ~= "table" then
		serviceMatches = {}
		ctx.matchedSettingsInstancesByService[serviceName] = serviceMatches
	end
	serviceMatches[settingsId] = instance
	if type(ctx.settingsIdLookupByService) == "table" then
		local cached = ctx.settingsIdLookupByService[serviceName]
		if type(cached) == "table" and type(cached.lookup) == "table" then
			cached.lookup[settingsId] = instance
		end
	end
end

local function clearMatchedSettingsInstances(serviceName: string, ctx: { [string]: any })
	if type(ctx.matchedSettingsInstancesByService) == "table" then
		ctx.matchedSettingsInstancesByService[serviceName] = nil
	end
	if type(ctx.settingsIdLookupByService) == "table" then
		ctx.settingsIdLookupByService[serviceName] = nil
	end
end

local function getStateForService(serviceName: string, ctx: { [string]: any }): any
	if serviceName == "" or type(ctx.getState) ~= "function" then
		return nil
	end
	return ctx.getState(serviceName)
end

local function parseInstanceIndexId(settingsId: string, identityModule: any): number?
	if type(identityModule) == "table" and type(identityModule.parseInstanceIndexId) == "function" then
		local index = identityModule.parseInstanceIndexId(settingsId)
		if type(index) == "number" then
			return index
		end
	end
	return tonumber(settingsId, 16)
end

local function settingsIdLookupForService(serviceName: string, ctx: { [string]: any })
	if serviceName == "" then
		return nil, nil
	end
	if type(ctx.settingsIdLookupByService) ~= "table" then
		ctx.settingsIdLookupByService = {}
	end
	local cached = ctx.settingsIdLookupByService[serviceName]
	if type(cached) == "table" then
		return cached.lookup, cached.state
	end

	local lookup = {}
	local state = getStateForService(serviceName, ctx)
	if state ~= nil and type(state.instances) == "table" then
		local identityModule = ctx.identityModule
		for index, candidate in ipairs(state.instances) do
			local instance = liveInstance(candidate)
			if instance ~= nil then
				lookup[string.format("%x", index)] = instance
				if type(state.instanceIdByInstance) == "table" then
					local instanceId = state.instanceIdByInstance[instance]
					if type(instanceId) == "number" and instanceId >= 1 then
						lookup[string.format("%x", instanceId)] = instance
					elseif type(instanceId) == "string" and instanceId ~= "" then
						lookup[instanceId] = instance
					end
				end
				if type(identityModule) == "table" and type(identityModule.getCachedDebugId) == "function" then
					local debugId = identityModule.getCachedDebugId(state, instance)
					if type(debugId) == "string" and debugId ~= "" then
						lookup["debug:" .. debugId] = instance
					end
				end
			end
		end
	end
	if type(ctx.matchedSettingsInstancesByService) == "table" then
		local serviceMatches = ctx.matchedSettingsInstancesByService[serviceName]
		if type(serviceMatches) == "table" then
			for settingsId, candidate in pairs(serviceMatches) do
				local instance = liveInstance(candidate)
				if instance ~= nil then
					lookup[settingsId] = instance
				else
					serviceMatches[settingsId] = nil
				end
			end
		end
	end
	ctx.settingsIdLookupByService[serviceName] = {
		lookup = lookup,
		state = state,
	}
	return lookup, state
end

local function resolveInstanceBySettingsId(serviceName: string, rawSettingsId: any, ctx: { [string]: any }): Instance?
	local settingsId = settingsIdText(rawSettingsId)
	if settingsId == nil then
		return nil
	end
	local lookup = settingsIdLookupForService(serviceName, ctx)
	if type(lookup) ~= "table" then
		return nil
	end

	local instance = liveInstance(lookup[settingsId])
	if instance ~= nil then
		return instance
	end
	local index = parseInstanceIndexId(settingsId, ctx.identityModule)
	if index ~= nil and index >= 1 then
		instance = liveInstance(lookup[string.format("%x", index)])
		if instance ~= nil then
			return instance
		end
	end
	return nil
end

local function instanceMatchesExpectedClass(instance: Instance, expectedClassName: any): boolean
	local className = tostring(expectedClassName or "")
	return className == "" or instance.ClassName == className
end

local function resolveInstance(change: { [string]: any }, ctx: { [string]: any }, allowClassMismatch: boolean?): Instance?
	local serviceName = tostring(change.service or "")
	local pathSegments = change.pathSegments
	if type(pathSegments) == "table" and #pathSegments > 0 then
		if #pathSegments == 1 and tostring(pathSegments[1]) == serviceName then
			local service = game:GetService(serviceName)
			if service ~= nil and instanceMatchesExpectedClass(service, change.className) then
				return service
			end
		end
	end
	local persistent = matchedSettingsInstance(serviceName, change.settingsId, ctx)
	if
		persistent ~= nil
		and (allowClassMismatch or instanceMatchesExpectedClass(persistent, change.className))
	then
		return persistent
	end
	local instance = resolveInstanceBySettingsId(serviceName, change.settingsId, ctx)
	if
		instance ~= nil
		and strongSettingsId(change.settingsId)
		and (allowClassMismatch or instanceMatchesExpectedClass(instance, change.className))
	then
		return instance
	end
	if type(pathSegments) == "table" and #pathSegments > 0 then
		local pathInstance = resolvePathSegments(pathSegments, ctx.resolveCache, change.pathOrdinals)
		if pathInstance ~= nil and (allowClassMismatch or instanceMatchesExpectedClass(pathInstance, change.className)) then
			return pathInstance
		end
	end
	if
		instance ~= nil
		and (allowClassMismatch or instanceMatchesExpectedClass(instance, change.className))
	then
		return instance
	end
	return nil
end

local function parentPathOrdinals(pathOrdinals: any): { any }
	local out = {}
	if type(pathOrdinals) ~= "table" then
		return out
	end
	for i = 1, #pathOrdinals - 1 do
		out[i] = pathOrdinals[i]
	end
	return out
end

local function resolveParent(change: { [string]: any }, resolveCache: { [string]: any }?): Instance?
	local pathSegments = cloneArray(change.pathSegments)
	if #pathSegments <= 1 then
		return nil
	end
	table.remove(pathSegments, #pathSegments)
	return resolvePathSegments(pathSegments, resolveCache, parentPathOrdinals(change.pathOrdinals))
end

local function parentPathSegments(pathSegments: { any }): { any }
	local out = table.create(math.max(#pathSegments - 1, 0))
	for i = 1, #pathSegments - 1 do
		out[i] = tostring(pathSegments[i])
	end
	return out
end

local function entryParentKey(entry: { [string]: any }): string
	return pathCacheKey(parentPathSegments(entry.pathSegments), parentPathOrdinals(entry.pathOrdinals))
end

local function resolveEntryParent(entry: { [string]: any }, resolvedEntries: { [string]: any }?): Instance?
	local parentKey = entryParentKey(entry)
	if type(resolvedEntries) == "table" then
		local parent = liveInstance(resolvedEntries[parentKey])
		if parent ~= nil then
			return parent
		end
	end
	return resolvePathSegments(parentPathSegments(entry.pathSegments), nil, parentPathOrdinals(entry.pathOrdinals))
end

local function syncEntryPlacement(entry: { [string]: any }, instance: Instance, stats: { [string]: any }, resolvedEntries: { [string]: any }?)
	local parent = resolveEntryParent(entry, resolvedEntries)
	if parent == nil then
		error("Cannot place instance; parent path was not found: " .. tostring(entry.key))
	end
	if instance.Parent ~= parent then
		instance.Parent = parent
		stats.propertyUpdated += 1
	end
	local nextName = tostring(entry.pathSegments[#entry.pathSegments] or instance.Name)
	if nextName ~= "" and instance.Name ~= nextName then
		instance.Name = nextName
		stats.propertyUpdated += 1
	end
end

local function instanceSettingsIdKeys(serviceName: string, instance: Instance, ctx: { [string]: any }): { [string]: boolean }
	local keys = {}
	local state = getStateForService(serviceName, ctx)
	if state == nil then
		return keys
	end
	if type(state.instanceIdByInstance) == "table" then
		local instanceId = state.instanceIdByInstance[instance]
		if type(instanceId) == "number" and instanceId >= 1 then
			keys[string.format("%x", instanceId)] = true
		elseif type(instanceId) == "string" and instanceId ~= "" then
			keys[instanceId] = true
		end
	end
	if type(state.instanceIndexByInstance) == "table" then
		local instanceIndex = state.instanceIndexByInstance[instance]
		if type(instanceIndex) == "number" and instanceIndex >= 1 then
			keys[string.format("%x", instanceIndex)] = true
		end
	end
	local identityModule = ctx.identityModule
	if type(identityModule) == "table" then
		if type(identityModule.getCachedInstanceIndex) == "function" then
			local index = identityModule.getCachedInstanceIndex(state, instance)
			if type(index) == "number" and index >= 1 then
				keys[string.format("%x", index)] = true
			end
		end
		if type(identityModule.getCachedDebugId) == "function" then
			local debugId = identityModule.getCachedDebugId(state, instance)
			if type(debugId) == "string" and debugId ~= "" then
				keys["debug:" .. debugId] = true
			end
		end
	end
	return keys
end

local function rememberReplacementIdentity(
	serviceName: string,
	rawSettingsId: any,
	oldInstance: Instance,
	replacement: Instance,
	ctx: { [string]: any }
)
	rememberMatchedSettingsInstance(serviceName, rawSettingsId, replacement, ctx)
	local state = getStateForService(serviceName, ctx)
	if state == nil then
		return
	end
	local keys = instanceSettingsIdKeys(serviceName, oldInstance, ctx)
	local settingsId = settingsIdText(rawSettingsId)
	if settingsId ~= nil then
		keys[settingsId] = true
	end

	local parsedIndex = if settingsId ~= nil then parseInstanceIndexId(settingsId, ctx.identityModule) else nil
	local index = if type(parsedIndex) == "number" and parsedIndex >= 1 then parsedIndex else nil
	if type(state.instances) == "table" then
		local indexedInstance = if index ~= nil then liveInstance(state.instances[index]) else nil
		if index == nil or indexedInstance ~= oldInstance then
			for candidateIndex, candidate in ipairs(state.instances) do
				if candidate == oldInstance then
					index = candidateIndex
					break
				end
			end
		end
		if index ~= nil then
			state.instances[index] = replacement
			keys[string.format("%x", index)] = true
		end
	end

	if type(state.instanceIdByInstance) == "table" then
		local oldId = state.instanceIdByInstance[oldInstance]
		if oldId ~= nil then
			state.instanceIdByInstance[replacement] = oldId
		end
	end
	if type(state.instanceIndexByInstance) == "table" then
		local oldIndex = state.instanceIndexByInstance[oldInstance]
		if oldIndex ~= nil then
			state.instanceIndexByInstance[replacement] = oldIndex
		elseif index ~= nil then
			state.instanceIndexByInstance[replacement] = index
		end
	end
	if type(state.pathByInstance) == "table" then
		state.pathByInstance[replacement] = nil
	end
	if type(state.pathSegmentsByInstance) == "table" then
		state.pathSegmentsByInstance[replacement] = nil
	end

	if type(ctx.settingsIdLookupByService) == "table" then
		local cached = ctx.settingsIdLookupByService[serviceName]
		if type(cached) == "table" and type(cached.lookup) == "table" then
			for key in pairs(keys) do
				cached.lookup[key] = replacement
			end
		end
	end
end

local function recordDesiredStableEntry(
	entry: { [string]: any },
	serviceName: string,
	instance: Instance?,
	ctx: { [string]: any },
	desiredSettingsIds: { [string]: boolean },
	desiredStableKeys: { [string]: boolean }
)
	local settingsId = entry.settingsId
	if settingsId == nil then
		return
	end
	if instance == nil then
		return
	end
	desiredSettingsIds[settingsId] = true
	for key in pairs(instanceSettingsIdKeys(serviceName, instance, ctx)) do
		desiredSettingsIds[key] = true
	end
	desiredStableKeys[entry.key] = true
end

local function instanceMatchesDesiredSettingsId(
	serviceName: string,
	instance: Instance,
	ctx: { [string]: any },
	desiredSettingsIds: { [string]: boolean }
): boolean
	if next(desiredSettingsIds) == nil then
		return false
	end
	for key in pairs(instanceSettingsIdKeys(serviceName, instance, ctx)) do
		if desiredSettingsIds[key] then
			return true
		end
	end
	return false
end

local function shouldKeepInstanceByDesiredEntry(
	serviceName: string,
	instance: Instance,
	pathSegments: any,
	pathOrdinals: any,
	ctx: { [string]: any },
	desiredKeys: { [string]: boolean },
	desiredSettingsIds: { [string]: boolean },
	desiredStableKeys: { [string]: boolean }
): boolean
	if instanceMatchesDesiredSettingsId(serviceName, instance, ctx, desiredSettingsIds) then
		return true
	end
	local key = pathSegments and pathCacheKey(pathSegments, pathOrdinals) or ""
	local legacyKey = pathSegments and pathKey(pathSegments) or ""
	if key ~= "" and desiredStableKeys[key] then
		return false
	end
	return key ~= "" and (desiredKeys[key] or desiredKeys[legacyKey]) or false
end

local function getInstancePathSegments(instance: Instance): { string }?
	if instance == game or not instance:IsDescendantOf(game) then
		return nil
	end
	local segments = {}
	local current: Instance? = instance
	while current ~= nil and current ~= game do
		table.insert(segments, 1, current.Name)
		current = current.Parent
	end
	return segments
end

local function getSiblingNameOrdinal(instance: Instance): number
	local parent = instance.Parent
	if parent == nil then
		return 1
	end
	local ordinal = 0
	for _, child in ipairs(parent:GetChildren()) do
		if child.Name == instance.Name then
			ordinal += 1
			if child == instance then
				return ordinal
			end
		end
	end
	return 1
end

local function getInstancePathOrdinals(instance: Instance): { number }?
	if instance == game or not instance:IsDescendantOf(game) then
		return nil
	end
	local ordinals = {}
	local current: Instance? = instance
	while current ~= nil and current ~= game do
		table.insert(ordinals, 1, getSiblingNameOrdinal(current))
		current = current.Parent
	end
	return ordinals
end

local function getServiceRoot(serviceName: string): Instance?
	if serviceName == "" then
		return nil
	end
	return game:GetService(serviceName)
end

local function assertAllowedService(serviceName: string, ctx: { [string]: any }): Instance
	if serviceName == "" or type(ctx.allowedServices) ~= "table" or not ctx.allowedServices[serviceName] then
		error("Refusing editor mutation outside an allowed service: " .. tostring(serviceName))
	end
	local service = getServiceRoot(serviceName)
	if service == nil then
		error("Service was not found: " .. serviceName)
	end
	return service
end

local function validateChangePath(change: { [string]: any }, serviceName: string, ctx: { [string]: any })
	local pathSegments = change.pathSegments
	if pathSegments == nil then
		return
	end
	if type(pathSegments) ~= "table" or #pathSegments == 0 then
		error("Editor mutation path must be a non-empty array")
	end
	if #pathSegments > (tonumber(ctx.maxPathSegments) or 128) then
		error("Editor mutation path has too many segments")
	end
	if tostring(pathSegments[1]) ~= serviceName then
		error("Editor mutation path root does not match service: " .. pathKey(pathSegments))
	end
	for _, segment in ipairs(pathSegments) do
		if type(segment) ~= "string" or segment == "" or #segment > 255 then
			error("Editor mutation path contains an invalid segment")
		end
	end
end

local function validatedChangeService(change: any, ctx: { [string]: any }): (string, Instance)
	if type(change) ~= "table" then
		error("Editor mutation entry must be an object")
	end
	local serviceName = tostring(change.service or "")
	local service = assertAllowedService(serviceName, ctx)
	validateChangePath(change, serviceName, ctx)
	return serviceName, service
end

local function assertInstanceInService(instance: Instance, service: Instance)
	if instance ~= service and not instance:IsDescendantOf(service) then
		error("Refusing editor mutation outside service root: " .. instance:GetFullName())
	end
end

local function isProtectedWorkspaceCameraPath(pathSegments: any): boolean
	if type(pathSegments) ~= "table" or #pathSegments ~= 2 then
		return false
	end
	local rootName = tostring(pathSegments[1])
	local cameraName = tostring(pathSegments[2])
	return rootName == "Workspace" and (cameraName == "Camera" or cameraName == "CurrentCamera")
end

local function isProtectedWorkspaceCameraInstance(instance: Instance?): boolean
	if instance == nil or not instance:IsA("Camera") then
		return false
	end
	if instance.Parent ~= Workspace then
		return false
	end
	return instance.Name == "Camera" or instance.Name == "CurrentCamera"
end

local function decodeNumberFields(raw: any, fields: { any }): (boolean, any)
	if type(raw) ~= "table" then
		return false, "Typed numeric value must be an object"
	end
	local values = table.create(#fields)
	for index, field in ipairs(fields) do
		local names = if type(field) == "table" then field else { field }
		local rawValue = nil
		for _, name in ipairs(names) do
			if raw[name] ~= nil then
				rawValue = raw[name]
				break
			end
		end
		local value = BridgeValueCodec.decodeNumber(rawValue)
		if not value then
			return false, tostring(names[1]) .. " must be a number"
		end
		values[index] = value
	end
	return true, values
end

local function decodeColor3(raw: any): (boolean, any)
	if typeof(raw) == "Color3" then
		return true, raw
	end
	local ok, values = decodeNumberFields(raw, {
		{ "r", "R", 1 },
		{ "g", "G", 2 },
		{ "b", "B", 3 },
	})
	if not ok then
		return false, "Color3 " .. tostring(values)
	end
	return true, Color3.new(values[1], values[2], values[3])
end

local function decodeEnumItem(raw: { [string]: any }, enumHint: string?): (boolean, any)
	local rawEnumType = tostring(raw.enumType or "")
	local enumType = if rawEnumType ~= "" or not enumHint or enumHint == ""
		then rawEnumType
		elseif string.sub(enumHint, 1, 5) == "Enum."
		then enumHint
		else "Enum." .. enumHint
	local enumName = string.gsub(enumType, "^Enum%.", "")
	local itemName = tostring(raw.name or "")
	local ok, item = pcall(function()
		return (Enum :: any)[enumName][itemName]
	end)
	if ok and item ~= nil then
		return true, item
	end
	return false, ("Unknown enum item %s.%s"):format(enumType, itemName)
end

local function decodeRefValue(raw: { [string]: any }, ctx: { [string]: any }?, serviceName: string?): any
	local targetServiceName = if type(raw.pathSegments) == "table" and #raw.pathSegments > 0
		then tostring(raw.pathSegments[1])
		else serviceName
	local settingsInstance: Instance? = nil
	if type(ctx) == "table" and type(targetServiceName) == "string" and targetServiceName ~= "" then
		local settingsId = raw.settingsId or raw.instanceId
		if settingsId == nil and type(raw.debugId) == "string" and raw.debugId ~= "" then
			settingsId = "debug:" .. raw.debugId
		end
		local persistent = matchedSettingsInstance(targetServiceName, settingsId, ctx)
		if persistent ~= nil then
			return persistent
		end
		settingsInstance = resolveInstanceBySettingsId(targetServiceName, settingsId, ctx)
		if settingsInstance ~= nil and strongSettingsId(settingsId) then
			return settingsInstance
		end
	end
	if type(raw.pathSegments) == "table" then
		local instance = resolvePathSegments(raw.pathSegments, nil, raw.pathOrdinals)
		if instance ~= nil then
			return instance
		end
	end
	return settingsInstance
end

local function decodeValue(raw: any, enumHint: string?, ctx: { [string]: any }?, serviceName: string?): (boolean, any)
	if type(raw) ~= "table" then
		return true, raw
	end

	local typeName = raw._type
	if typeName == nil and enumHint == "FontFace" and raw.family ~= nil then
		typeName = "Font"
	elseif typeName == nil and raw.BrickColor ~= nil then
		typeName = "BrickColor"
	elseif typeName == nil and type(raw.ColorSequence) == "table" then
		raw = raw.ColorSequence
		typeName = "ColorSequence"
	elseif typeName == nil and type(raw.NumberSequence) == "table" then
		raw = raw.NumberSequence
		typeName = "NumberSequence"
	elseif typeName == nil and type(raw.NumberRange) == "table" then
		raw = raw.NumberRange
		typeName = "NumberRange"
	elseif typeName == nil and type(raw.Ref) == "table" then
		raw = raw.Ref
		typeName = "Ref"
	elseif typeName == nil and raw.customPhysics ~= nil then
		typeName = "PhysicalProperties"
	end
	if typeName == nil then
		return true, raw
	end
	typeName = tostring(typeName)

	if typeName == "Float" then
		local value = BridgeValueCodec.decodeNumber(raw)
		if not value then
			return false, "Float value must be a number or non-finite marker"
		end
		return true, value
	elseif typeName == "BinaryString" then
		local encoded = raw.base64
		if type(encoded) ~= "string" then
			return false, "BinaryString base64 must be a string"
		end
		local ok, decoded = pcall(EncodingService.Base64Decode, EncodingService, buffer.fromstring(encoded))
		if not ok then
			return false, decoded
		end
		return true, buffer.tostring(decoded)
	elseif typeName == "PhysicalProperties" then
		if raw.customPhysics == false or raw.density == nil then
			return true, nil
		end
		local okNumbers, values = decodeNumberFields(raw, {
			"density",
			"friction",
			"elasticity",
			"frictionWeight",
			"elasticityWeight",
		})
		if not okNumbers then
			return false, "PhysicalProperties " .. tostring(values)
		end
		if raw.acousticAbsorption ~= nil then
			local acousticAbsorption = tonumber(raw.acousticAbsorption)
			if not acousticAbsorption then
				return false, "PhysicalProperties acousticAbsorption must be a number"
			end
			local okCreate, physicalProperties = pcall(
				PhysicalProperties.new :: any,
				values[1],
				values[2],
				values[3],
				values[4],
				values[5],
				acousticAbsorption
			)
			if not okCreate then
				return false, physicalProperties
			end
			return true, physicalProperties
		end
		return true, PhysicalProperties.new(values[1], values[2], values[3], values[4], values[5])
	elseif typeName == "NumberRange" then
		local okNumbers, values = decodeNumberFields(raw, {
			{ "min", "Min", 1 },
			{ "max", "Max", 2 },
		})
		if not okNumbers then
			return false, "NumberRange " .. tostring(values)
		end
		return true, NumberRange.new(values[1], values[2])
	elseif typeName == "Vector2" then
		local okNumbers, values = decodeNumberFields(raw, { "x", "y" })
		if not okNumbers then
			return false, "Vector2 " .. tostring(values)
		end
		return true, Vector2.new(values[1], values[2])
	elseif typeName == "Vector3" then
		local okNumbers, values = decodeNumberFields(raw, { "x", "y", "z" })
		if not okNumbers then
			return false, "Vector3 " .. tostring(values)
		end
		return true, Vector3.new(values[1], values[2], values[3])
	elseif typeName == "UDim" then
		local okNumbers, values = decodeNumberFields(raw, { "scale", "offset" })
		if not okNumbers then
			return false, "UDim " .. tostring(values)
		end
		return true, UDim.new(values[1], values[2])
	elseif typeName == "UDim2" then
		local okNumbers, values = decodeNumberFields(raw, { "xScale", "xOffset", "yScale", "yOffset" })
		if not okNumbers then
			return false, "UDim2 " .. tostring(values)
		end
		return true, UDim2.new(values[1], values[2], values[3], values[4])
	elseif typeName == "Color3" then
		return decodeColor3(raw)
	elseif typeName == "BrickColor" then
		local number = tonumber(raw.number or raw.BrickColor)
		if not number then
			return false, "BrickColor number must be numeric"
		end
		return true, BrickColor.new(number)
	elseif typeName == "EnumItem" then
		return decodeEnumItem(raw, enumHint)
	elseif typeName == "CFrame" then
		local components = raw.components
		if type(components) ~= "table" or #components ~= 12 then
			return false, "CFrame components must contain 12 numbers"
		end
		local values = table.create(12)
		for i = 1, 12 do
			local component = BridgeValueCodec.decodeNumber(components[i])
			if not component then
				return false, string.format("CFrame component %d must be a number", i)
			end
			values[i] = component
		end
		return true, CFrame.new(table.unpack(values))
	elseif typeName == "Rect" then
		local okNumbers, values = decodeNumberFields(raw, { "minX", "minY", "maxX", "maxY" })
		if not okNumbers then
			return false, "Rect " .. tostring(values)
		end
		return true, Rect.new(values[1], values[2], values[3], values[4])
	elseif typeName == "Font" then
		local ok, font = pcall(function()
			return Font.new(
				tostring(raw.family or ""),
				(Enum.FontWeight :: any)[tostring(raw.weight or "Regular")],
				(Enum.FontStyle :: any)[tostring(raw.style or "Normal")]
			)
		end)
		if ok then
			return true, font
		end
		return false, font
	elseif typeName == "ColorSequence" then
		local keypoints = raw.keypoints
		if type(keypoints) ~= "table" then
			return false, "ColorSequence keypoints must be a table"
		end
		local decoded = table.create(#keypoints)
		for i, keypoint in ipairs(keypoints) do
			if type(keypoint) ~= "table" then
				return false, string.format("ColorSequence keypoint %d must be an object", i)
			end
			local colorRaw = if keypoint.value ~= nil then keypoint.value else keypoint.color or keypoint.Value
			local okColor, color = decodeColor3(colorRaw)
			if not okColor then
				return false, color
			end
			local okNumbers, values = decodeNumberFields(keypoint, { "time" })
			if not okNumbers then
				return false, string.format("ColorSequence keypoint %d %s", i, tostring(values))
			end
			local okKeypoint, decodedKeypoint = pcall(ColorSequenceKeypoint.new, values[1], color)
			if not okKeypoint then
				return false, decodedKeypoint
			end
			decoded[i] = decodedKeypoint
		end
		local okSequence, sequence = pcall(ColorSequence.new, decoded)
		return okSequence, sequence
	elseif typeName == "NumberSequence" then
		local keypoints = raw.keypoints
		if type(keypoints) ~= "table" then
			return false, "NumberSequence keypoints must be a table"
		end
		local decoded = table.create(#keypoints)
		for i, keypoint in ipairs(keypoints) do
			if type(keypoint) ~= "table" then
				return false, string.format("NumberSequence keypoint %d must be an object", i)
			end
			local okNumbers, values = decodeNumberFields(keypoint, { "time", "value", "envelope" })
			if not okNumbers then
				return false, string.format("NumberSequence keypoint %d %s", i, tostring(values))
			end
			local okKeypoint, decodedKeypoint = pcall(
				NumberSequenceKeypoint.new,
				values[1],
				values[2],
				values[3]
			)
			if not okKeypoint then
				return false, decodedKeypoint
			end
			decoded[i] = decodedKeypoint
		end
		local okSequence, sequence = pcall(NumberSequence.new, decoded)
		return okSequence, sequence
	elseif typeName == "Axes" then
		if type(raw.axes) ~= "table" then
			return false, "Axes axes must be an array"
		end
		local axes = {}
		for _, name in ipairs(raw.axes) do
			local item = (Enum.Axis :: any)[tostring(name)]
			if item == nil then
				return false, "Unknown axis " .. tostring(name)
			end
			axes[#axes + 1] = item
		end
		return true, Axes.new(table.unpack(axes))
	elseif typeName == "Faces" then
		if type(raw.faces) ~= "table" then
			return false, "Faces faces must be an array"
		end
		local faces = {}
		for _, name in ipairs(raw.faces) do
			local item = (Enum.NormalId :: any)[tostring(name)]
			if item == nil then
				return false, "Unknown face " .. tostring(name)
			end
			faces[#faces + 1] = item
		end
		return true, Faces.new(table.unpack(faces))
	elseif typeName == "Ray" then
		local okOrigin, origin = decodeNumberFields(raw.origin, { "x", "y", "z" })
		if not okOrigin then
			return false, "Ray origin " .. tostring(origin)
		end
		local okDirection, direction = decodeNumberFields(raw.direction, { "x", "y", "z" })
		if not okDirection then
			return false, "Ray direction " .. tostring(direction)
		end
		return true, Ray.new(
			Vector3.new(origin[1], origin[2], origin[3]),
			Vector3.new(direction[1], direction[2], direction[3])
		)
	elseif typeName == "Ref" then
		return true, decodeRefValue(raw, ctx, serviceName)
	end

	return true, raw
end

local function enumHintForProperty(instance: Instance, propertyName: string): string?
	local descriptor = RbxDomModule.findCanonicalPropertyDescriptor(instance.ClassName, propertyName)
	if descriptor ~= nil and type(descriptor.enumType) == "string" and descriptor.enumType ~= "" then
		return descriptor.enumType
	end
	local okRead, current = pcall(function()
		return (instance :: any)[propertyName]
	end)
	if okRead and typeof(current) == "EnumItem" then
		return tostring(current.EnumType)
	end
	return propertyName
end

local function classHasProperty(instance: Instance, propertyName: string): boolean
	if instance:IsA("Model") or instance:IsA("WorldModel") then
		if propertyName == "Scale" or propertyName == "WorldPivot" or propertyName == "WorldPivotData" or propertyName == "Origin" then
			return true
		end
	end
	return RbxDomModule.findCanonicalPropertyDescriptor(instance.ClassName, propertyName) ~= nil
end

local function decodePropertyValue(instance: Instance, propertyName: string, rawValue: any, ctx: { [string]: any }, serviceName: string): (boolean, any)
	if type(rawValue) == "table" and rawValue._type == nil then
		local okCurrent, current = pcall(function()
			return (instance :: any)[propertyName]
		end)
		if okCurrent and typeof(current) == "NumberRange" then
			local okNumbers, values = decodeNumberFields(rawValue, {
				{ "min", "Min", 1 },
				{ "max", "Max", 2 },
			})
			if not okNumbers then
				return false, "NumberRange " .. tostring(values)
			end
			return true, NumberRange.new(values[1], values[2])
		end
	end
	return decodeValue(rawValue, enumHintForProperty(instance, propertyName), ctx, serviceName)
end

local valuesEqual = BridgeValueEquality.valuesEqual

BridgeEditorSync.decodeValue = decodeValue
BridgeEditorSync.valuesEqual = valuesEqual

local function connectProbeSignal(stats: { [string]: any }, eventName: string, countField: string, availableField: string, connections: { RBXScriptConnection })
	local signal = (game :: any)[eventName]
	if not signal then
		return
	end
	local connection = signal:Connect(function()
		stats[countField] += 1
	end)
	stats[availableField] = 1
	table.insert(connections, connection)
end

local function startEventProbe(stats: { [string]: any }): () -> ()
	local connections = {}
	connectProbeSignal(stats, "ItemChanged", "probeItemChanged", "probeItemChangedAvailable", connections)
	connectProbeSignal(stats, "DescendantAdded", "probeDescendantAdded", "probeDescendantAddedAvailable", connections)
	connectProbeSignal(stats, "DescendantRemoving", "probeDescendantRemoving", "probeDescendantRemovingAvailable", connections)

	return function()
		for _, connection in ipairs(connections) do
			connection:Disconnect()
		end
	end
end

local function readProperty(instance: Instance, propertyName: string): (boolean, any)
	if instance:IsA("Model") or instance:IsA("WorldModel") then
		if propertyName == "Scale" then
			return true, (instance :: any):GetScale()
		elseif propertyName == "WorldPivot" or propertyName == "WorldPivotData" or propertyName == "Origin" then
			return true, (instance :: any):GetPivot()
		end
	end
	local okCall, okRead, value = pcall(RbxDomModule.readProperty, instance, propertyName)
	if okCall and okRead then
		return true, value
	end
	return pcall(function()
		return (instance :: any)[propertyName]
	end)
end

local function candidateBucketsForParent(parent: Instance, ctx: { [string]: any }): { [string]: any }
	if type(ctx.matchCandidateBuckets) ~= "table" then
		ctx.matchCandidateBuckets = {}
	end
	local cached = ctx.matchCandidateBuckets[parent]
	if type(cached) == "table" then
		return cached
	end
	local buckets = {}
	for _, child in ipairs(parent:GetChildren()) do
		local name = child.Name
		local className = child.ClassName
		local byClass = buckets[name]
		if byClass == nil then
			byClass = {}
			buckets[name] = byClass
		end
		local candidates = byClass[className]
		if candidates == nil then
			candidates = {}
			byClass[className] = candidates
		end
		candidates[#candidates + 1] = child
	end
	ctx.matchCandidateBuckets[parent] = buckets
	return buckets
end

local function rememberEntryResolution(
	entry: { [string]: any },
	serviceName: string,
	instance: Instance,
	claimedInstances: { [Instance]: boolean },
	ctx: { [string]: any }
)
	claimedInstances[instance] = true
	if entry.ambiguousSiblings then
		rememberMatchedSettingsInstance(serviceName, entry.settingsId, instance, ctx)
	end
end

local function resolveEntryInstance(
	entry: { [string]: any },
	serviceName: string,
	ctx: { [string]: any },
	resolvedEntries: { [string]: any },
	claimedInstances: { [Instance]: boolean }
): Instance?
	local persistent = matchedSettingsInstance(serviceName, entry.settingsId, ctx)
	if persistent ~= nil and not claimedInstances[persistent] then
		return persistent
	end

	local settingsInstance = resolveInstanceBySettingsId(serviceName, entry.settingsId, ctx)
	if
		settingsInstance ~= nil
		and not claimedInstances[settingsInstance]
		and strongSettingsId(entry.settingsId)
	then
		return settingsInstance
	end

	local pathInstance = resolvePathSegments(entry.pathSegments, nil, entry.pathOrdinals)
	if not entry.ambiguousSiblings then
		if pathInstance ~= nil and not claimedInstances[pathInstance] then
			return pathInstance
		end
		if settingsInstance ~= nil and not claimedInstances[settingsInstance] then
			return settingsInstance
		end
		return nil
	end

	local parent = resolveEntryParent(entry, resolvedEntries)
	local expectedName = tostring(entry.pathSegments[#entry.pathSegments] or "")
	local candidates = {}
	local included = {}
	local function include(candidate: Instance?)
		if not candidate or claimedInstances[candidate] or included[candidate] then
			return
		end
		if parent ~= nil and candidate.Parent ~= parent then
			return
		end
		included[candidate] = true
		candidates[#candidates + 1] = candidate
	end
	include(pathInstance)
	if parent ~= nil then
		local byClass = candidateBucketsForParent(parent, ctx)[expectedName]
		local bucket = byClass and byClass[entry.className]
		if type(bucket) == "table" then
			for _, candidate in ipairs(bucket) do
				include(candidate)
			end
		end
	end
	include(settingsInstance)

	return BridgeCandidateMatch.choose(
		candidates,
		entry.matchProperties,
		entry.matchAttributes,
		function(candidate, propertyName, rawValue)
			if not classHasProperty(candidate, propertyName) then
				return false
			end
			local okRead, current = readProperty(candidate, propertyName)
			local okDecode, decoded = decodePropertyValue(candidate, propertyName, rawValue, ctx, serviceName)
			return okRead and okDecode and valuesEqual(current, decoded)
		end,
		function(candidate, attributeName, rawValue)
			local okDecode, decoded = decodeValue(rawValue, nil, ctx, serviceName)
			return okDecode and valuesEqual(candidate:GetAttribute(attributeName), decoded)
		end
	)
end

local function writeProperty(instance: Instance, propertyName: string, value: any): (boolean, any)
	if instance:IsA("Model") or instance:IsA("WorldModel") then
		if propertyName == "Scale" then
			return pcall(function()
				return (instance :: any):ScaleTo(value)
			end)
		elseif propertyName == "Origin" then
			return pcall(function()
				return (instance :: any):PivotTo(value)
			end)
		elseif propertyName == "WorldPivot" or propertyName == "WorldPivotData" then
			return pcall(function()
				(instance :: any).WorldPivot = value
			end)
		end
	end
	local okCall, okDomWrite = pcall(RbxDomModule.writeProperty, instance, propertyName, value)
	if okCall and okDomWrite then
		return true, nil
	end
	local okWrite, writeErr = pcall(function()
		(instance :: any)[propertyName] = value
	end)
	if okWrite then
		return true, nil
	end
	if type(value) == "string" and Content ~= nil and (Content :: any).fromUri ~= nil then
		local okContent = pcall(function()
			(instance :: any)[propertyName] = if value == ""
				then (Content :: any).none
				else (Content :: any).fromUri(value)
		end)
		if okContent then
			return true, nil
		end
	end
	return false, writeErr
end

local function applyMeshPartMeshId(instance: Instance, meshId: any): (boolean, any)
	if not instance:IsA("MeshPart") then
		return false, "MeshId fallback only supports MeshPart"
	end

	local meshIdText = tostring(meshId or "")
	if meshIdText == "" then
		return false, "Cannot apply empty MeshId through CreateMeshPartAsync"
	end

	local collisionFidelity = (instance :: MeshPart).CollisionFidelity
	local renderFidelity = (instance :: MeshPart).RenderFidelity

	local okCreate, meshPartOrErr = pcall(function()
		return InsertService:CreateMeshPartAsync(meshIdText, collisionFidelity, renderFidelity)
	end)
	if not okCreate or meshPartOrErr == nil then
		return false, meshPartOrErr
	end

	local sourceMeshPart = meshPartOrErr :: MeshPart
	local okApply, applyErr = pcall(function()
		(instance :: MeshPart):ApplyMesh(sourceMeshPart)
	end)
	sourceMeshPart:Destroy()
	if not okApply then
		return false, applyErr
	end
	return true, nil
end

local function canApplyProtectedMeshId(change: { [string]: any }, instance: Instance): boolean
	if change.allowProtectedMeshIdApply == true then
		return true
	end
	if not instance:IsA("MeshPart") then
		return false
	end
	return not not recentlyCreatedMeshParts[instance]
end

local function clearRecentlyCreatedMeshPart(instance: Instance)
	recentlyCreatedMeshParts[instance] = nil
end

local function applyTags(instance: Instance, rawTags: any, stats: { [string]: any })
	local desired = {}
	if type(rawTags) == "table" then
		for _, tag in pairs(rawTags) do
			if type(tag) == "string" and tag ~= "" then
				desired[tag] = true
			end
		end
	end

	local changed = false
	for _, tag in ipairs(CollectionService:GetTags(instance)) do
		if not desired[tag] then
			CollectionService:RemoveTag(instance, tag)
			changed = true
		end
		desired[tag] = nil
	end
	for tag in pairs(desired) do
		CollectionService:AddTag(instance, tag)
		changed = true
	end
	if changed then
		stats.propertyUpdated += 1
	else
		stats.noops += 1
	end
end

local function replaceInstanceClass(
	instance: Instance,
	className: string,
	stats: { [string]: any },
	selectionReplacements: { [Instance]: Instance }?
): Instance
	if instance.ClassName == className then
		stats.noops += 1
		return instance
	end

	local replacement = BridgeInstanceSwap.replace(instance, className, CollectionService, removeInstanceForUndo)
	if selectionReplacements ~= nil then
		selectionReplacements[instance] = replacement
	end
	stats.instanceReplaced += 1
	return replacement
end

local function retargetReplacementReferences(
	replacements: { [Instance]: Instance },
	ctx: { [string]: any },
	stats: { [string]: any }
)
	if next(replacements) == nil then
		return
	end
	local roots = {}
	for serviceName, allowed in pairs(ctx.allowedServices) do
		if allowed then
			roots[#roots + 1] = game:GetService(serviceName)
		end
	end
	local updated, failed = BridgeReferenceRetarget.apply(
		roots,
		replacements,
		RbxDomModule.getReferencePropertyNames,
		readProperty,
		writeProperty
	)
	stats.propertyUpdated += updated
	if failed > 0 then
		warn(`[Renium] could not retarget {failed} references after class replacement`)
	end
end

local function findScriptDocument(instance: Instance): any?
	local ok, document = pcall(function()
		return (ScriptEditorService :: any):FindScriptDocument(instance)
	end)
	if ok and document ~= nil then
		return document
	end
	return nil
end

local function readScriptSource(instance: Instance): (boolean, any)
	local okEditor, editorSource = pcall(function()
		return (ScriptEditorService :: any):GetEditorSource(instance)
	end)
	if okEditor then
		return true, editorSource
	end
	return pcall(function()
		return (instance :: any).Source
	end)
end

local function documentLineEndCharacter(document: any, line: number): number
	local okLine, lineText = pcall(function()
		return document:GetLine(line)
	end)
	if okLine and type(lineText) == "string" then
		return #lineText + 1
	end
	return 1
end

local function clampDocumentPosition(document: any, line: any, character: any): (number, number)
	local okCount, lineCount = pcall(function()
		return document:GetLineCount()
	end)
	if not okCount or type(lineCount) ~= "number" or lineCount < 1 then
		return 1, 1
	end
	local clampedLine = math.clamp(math.floor(tonumber(line) or 1), 1, lineCount)
	local lineEnd = documentLineEndCharacter(document, clampedLine)
	local clampedCharacter = math.clamp(math.floor(tonumber(character) or 1), 1, lineEnd)
	return clampedLine, clampedCharacter
end

local function getDocumentSelection(document: any): { number }?
	local okSelection, cursorLine, cursorCharacter, anchorLine, anchorCharacter = pcall(function()
		return document:GetSelection()
	end)
	if not okSelection or type(cursorLine) ~= "number" or type(cursorCharacter) ~= "number" then
		return nil
	end
	local resolvedAnchorLine = if type(anchorLine) == "number" then anchorLine else cursorLine
	local resolvedAnchorCharacter = if type(anchorCharacter) == "number" then anchorCharacter else cursorCharacter
	return { cursorLine, cursorCharacter, resolvedAnchorLine, resolvedAnchorCharacter }
end

local function restoreDocumentSelection(document: any, selection: { number }?)
	if selection == nil then
		return
	end
	local cursorLine, cursorCharacter = clampDocumentPosition(document, selection[1], selection[2])
	local anchorLine, anchorCharacter = clampDocumentPosition(document, selection[3], selection[4])
	local okRequest, success = pcall(function()
		return document:RequestSetSelectionAsync(cursorLine, cursorCharacter, anchorLine, anchorCharacter)
	end)
	if okRequest and success ~= false then
		return
	end
	pcall(function()
		document:ForceSetSelectionAsync(cursorLine, cursorCharacter, anchorLine, anchorCharacter)
	end)
end

local function setOpenDocumentSource(document: any, source: string): (boolean, any)
	local okText, currentText = pcall(function()
		return document:GetText()
	end)
	if okText and currentText == source then
		return true, nil
	end

	local okLineCount, lineCount = pcall(function()
		return document:GetLineCount()
	end)
	if not okLineCount or type(lineCount) ~= "number" or lineCount < 1 then
		return false, "open script document did not report a valid line count"
	end

	local selection = getDocumentSelection(document)
	local endCharacter = documentLineEndCharacter(document, lineCount)
	local okEdit, success, editErr = pcall(function()
		return document:EditTextAsync(source, 1, 1, lineCount, endCharacter)
	end)
	if not okEdit then
		return false, success
	end
	if success == false then
		return false, editErr or "EditTextAsync failed"
	end
	restoreDocumentSelection(document, selection)
	return true, nil
end

local function setSource(instance: Instance, source: string): (boolean, any, string)
	local document = findScriptDocument(instance)
	if document ~= nil then
		local documentOk, documentErr = setOpenDocumentSource(document, source)
		if documentOk then
			return true, nil, "ScriptDocument"
		end
	end

	local updateOk = pcall(function()
		(ScriptEditorService :: any):UpdateSourceAsync(instance, function()
			return source
		end)
	end)
	if updateOk then
		return true, nil, "UpdateSourceAsync"
	end
	local ok, err = pcall(function()
		(instance :: any).Source = source
	end)
	if ok then
		return true, nil, "Source"
	end
	return false, err, "Source"
end

local function syncOptions(ctx: { [string]: any }): { [string]: any }
	if type(ctx.syncOptions) == "table" then
		return ctx.syncOptions
	end
	if type(ctx.getSyncOptions) == "function" then
		local options = ctx.getSyncOptions()
		if type(options) == "table" then
			return options
		end
	end
	return {}
end

local function liveHydrateEnabled(ctx: { [string]: any }): boolean
	return syncOptions(ctx).liveHydrate ~= false
end

local function keepUnknownsEnabled(ctx: { [string]: any }): boolean
	return syncOptions(ctx).keepUnknowns == true
end

local function ensureSourceParentPath(change: { [string]: any }, service: Instance, stats: { [string]: any }): Instance?
	local pathSegments = change.pathSegments
	if type(pathSegments) ~= "table" or #pathSegments < 2 then
		return nil
	end
	local current = service
	for i = 2, #pathSegments - 1 do
		local name = tostring(pathSegments[i])
		local ordinal = if type(change.pathOrdinals) == "table" then tonumber(change.pathOrdinals[i]) or 1 else 1
		local child = resolveOrdinalChild(current, name, ordinal)
		if child == nil then
			local existing = 0
			for _, sibling in ipairs(current:GetChildren()) do
				if sibling.Name == name then
					existing += 1
				end
			end
			while existing < ordinal do
				local folder = Instance.new("Folder")
				folder.Name = name
				folder.Parent = current
				stats.instanceCreated += 1
				existing += 1
				child = folder
			end
		end
		current = child
	end
	return current
end

local function applySourceChange(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true
	if type(change.source) == "string" and #change.source > (tonumber(ctx.maxSourceBytes) or 8 * 1024 * 1024) then
		error("Editor source mutation exceeds safe size limit")
	end

	local instance = resolveInstance(change, ctx, true)
	if instance ~= nil then
		assertInstanceInService(instance, service)
	end
	if change.deleted == true then
		if instance ~= nil then
			if not ctx.luaSourceClass[instance.ClassName] then
				error("Target is not a Lua source container: " .. instance:GetFullName())
			end
			local okWrite, err, writeMethod = setSource(instance, "")
			if not okWrite then
				error(`Failed to clear Source for {instance:GetFullName()}: {err}`)
			end
			if writeMethod == "UpdateSourceAsync" then
				stats.sourceUpdateAsync += 1
			else
				stats.sourceDirect += 1
			end
			stats.sourceDeleted += 1
		else
			stats.noops += 1
		end
		return
	end

	if instance == nil then
		if not liveHydrateEnabled(ctx) then
			stats.noops += 1
			return
		end
		local parent = resolveParent(change, ctx.resolveCache)
		if parent == nil then
			parent = ensureSourceParentPath(change, service, stats)
		end
		if parent == nil then
			error("Cannot create source instance; parent path was not found")
		end
		assertInstanceInService(parent, service)
		local okCreate, created = pcall(Instance.new, tostring(change.className or "ModuleScript"))
		if not okCreate or created == nil then
			error("Cannot create source instance: " .. tostring(created))
		end
		local pathSegments = cloneArray(change.pathSegments)
		created.Name = tostring(pathSegments[#pathSegments] or created.ClassName)
		created.Parent = parent
		if type(ctx.resolveCache) == "table" then
			ctx.resolveCache[pathCacheKey(change.pathSegments, change.pathOrdinals)] = created
		end
		instance = created
		stats.sourceCreated += 1
	end

	if instance.ClassName == "Folder" and ctx.luaSourceClass[tostring(change.className or "")] then
		local oldInstance = instance
		instance = replaceInstanceClass(instance, tostring(change.className), stats, ctx.selectionReplacements)
		rememberReplacementIdentity(serviceName, change.settingsId, oldInstance, instance, ctx)
		if type(ctx.resolveCache) == "table" then
			ctx.resolveCache[pathCacheKey(change.pathSegments, change.pathOrdinals)] = instance
		end
	end
	if not ctx.luaSourceClass[instance.ClassName] then
		error("Target is not a Lua source container: " .. instance:GetFullName())
	end

	local nextSource = tostring(change.source or "")
	local okRead, currentSource = readScriptSource(instance)
	if okRead and currentSource == nextSource then
		stats.noops += 1
		return
	end

	local okWrite, err, writeMethod = setSource(instance, nextSource)
	if not okWrite then
		error(`Failed to write Source for {instance:GetFullName()}: {err}`)
	end
	if writeMethod == "UpdateSourceAsync" then
		stats.sourceUpdateAsync += 1
	else
		stats.sourceDirect += 1
	end
	stats.sourceUpdated += 1
end

local function applyInstanceReconcile(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true
	if tostring(change.mode or "reconcileService") ~= "reconcileService" then
		error("Unsupported instance sync mode: " .. tostring(change.mode))
	end
	clearMatchedSettingsInstances(serviceName, ctx)

	local rawInstances = change.instances
	if type(rawInstances) ~= "table" then
		return
	end
	if #rawInstances > (tonumber(ctx.maxInstanceEntriesPerChange) or 5000) then
		error("Editor instance reconcile has too many entries")
	end

	local beforeCreated = stats.instanceCreated
	local beforeDeleted = stats.instanceDeleted
	local beforeReplaced = stats.instanceReplaced
	local desiredKeys = {}
	local desiredSettingsIds = {}
	local desiredStableKeys = {}
	local desiredEntries = {}
	for _, raw in ipairs(rawInstances) do
		if type(raw) == "table" then
			local pathSegments = cloneArray(raw.pathSegments)
			local className = tostring(raw.className or "Folder")
			if #pathSegments > 0 and className ~= "PackageLink" then
				if tostring(pathSegments[1]) ~= service.Name then
					error("Instance path root does not match service: " .. pathKey(pathSegments))
				end
				local entry = {
					pathSegments = pathSegments,
					pathOrdinals = cloneArray(raw.pathOrdinals),
					key = pathCacheKey(pathSegments, raw.pathOrdinals),
					className = className,
					settingsId = settingsIdText(raw.settingsId),
					ambiguousSiblings = raw.ambiguousSiblings == true,
					matchProperties = if type(raw.matchProperties) == "table" then raw.matchProperties else {},
					matchAttributes = if type(raw.matchAttributes) == "table" then raw.matchAttributes else {},
				}
				desiredKeys[entry.key] = true
				table.insert(desiredEntries, entry)
			end
		end
	end

	table.sort(desiredEntries, function(a, b)
		if #a.pathSegments == #b.pathSegments then
			return a.key < b.key
		end
		return #a.pathSegments < #b.pathSegments
	end)

	local resolvedEntries = {}
	local claimedInstances = {}
	for _, entry in ipairs(desiredEntries) do
		if #entry.pathSegments > 1 then
			if isProtectedWorkspaceCameraPath(entry.pathSegments) then
				stats.noops += 1
			else
				local instance = resolveEntryInstance(entry, service.Name, ctx, resolvedEntries, claimedInstances)
				if instance == nil then
					local parent = resolveEntryParent(entry, resolvedEntries)
					if parent == nil then
						error("Cannot create instance; parent path was not found: " .. entry.key)
					end
					local okCreate, created = pcall(Instance.new, entry.className)
					if not okCreate or created == nil then
						error(`Cannot create {entry.className} at {pathKey(entry.pathSegments)}: {created}`)
					end
					created.Name = tostring(entry.pathSegments[#entry.pathSegments])
					created.Parent = parent
					if created:IsA("MeshPart") then
						recentlyCreatedMeshParts[created] = true
					end
					instance = created
					stats.instanceCreated += 1
				else
					syncEntryPlacement(entry, instance, stats, resolvedEntries)
					if instance.ClassName ~= entry.className then
						local oldInstance = instance
						instance = replaceInstanceClass(instance, entry.className, stats, ctx.selectionReplacements)
						rememberReplacementIdentity(service.Name, entry.settingsId, oldInstance, instance, ctx)
					end
				end
				resolvedEntries[entry.key] = instance
				rememberEntryResolution(entry, service.Name, instance, claimedInstances, ctx)
				recordDesiredStableEntry(entry, service.Name, instance, ctx, desiredSettingsIds, desiredStableKeys)
			end
		end
	end

	if change.allowDeletes == true and not keepUnknownsEnabled(ctx) then
		local descendants = service:GetDescendants()
		for i = #descendants, 1, -1 do
			local instance = descendants[i]
			local pathSegments = getInstancePathSegments(instance)
			local pathOrdinals = getInstancePathOrdinals(instance)
			local key = pathSegments and pathCacheKey(pathSegments, pathOrdinals) or ""
			if key ~= "" and not shouldKeepInstanceByDesiredEntry(service.Name, instance, pathSegments, pathOrdinals, ctx, desiredKeys, desiredSettingsIds, desiredStableKeys) and not isProtectedWorkspaceCameraInstance(instance) then
				removeInstanceForUndo(instance)
				stats.instanceDeleted += 1
			end
		end
	end

	if stats.instanceCreated == beforeCreated and stats.instanceDeleted == beforeDeleted and stats.instanceReplaced == beforeReplaced then
		stats.noops += 1
	end
end

local function sortedInstanceEntries(change: { [string]: any }, serviceName: string)
	local rawInstances = change.instances
	local entries = {}
	if type(rawInstances) ~= "table" then
		return entries
	end
	if #rawInstances > 5000 then
		error("Editor instance mutation has too many entries")
	end

	for _, raw in ipairs(rawInstances) do
		if type(raw) == "table" then
			local pathSegments = cloneArray(raw.pathSegments)
			local className = tostring(raw.className or "Folder")
			if #pathSegments > 0 then
				if tostring(pathSegments[1]) ~= serviceName then
					error("Instance path root does not match service: " .. pathKey(pathSegments))
				end
				if #pathSegments > 1 and className ~= serviceName and className ~= "PackageLink" then
					table.insert(entries, {
						pathSegments = pathSegments,
						pathOrdinals = cloneArray(raw.pathOrdinals),
						key = pathCacheKey(pathSegments, raw.pathOrdinals),
						className = className,
						settingsId = settingsIdText(raw.settingsId),
						ambiguousSiblings = raw.ambiguousSiblings == true,
						matchProperties = if type(raw.matchProperties) == "table" then raw.matchProperties else {},
						matchAttributes = if type(raw.matchAttributes) == "table" then raw.matchAttributes else {},
					})
				end
			end
		end
	end

	table.sort(entries, function(a, b)
		if #a.pathSegments == #b.pathSegments then
			return a.key < b.key
		end
		return #a.pathSegments < #b.pathSegments
	end)
	return entries
end

local function reconcileSessionKey(serviceName: string, sessionId: any): string
	return serviceName .. PATH_SEPARATOR .. tostring(sessionId or "default")
end

local function applyInstanceReconcileChunk(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true

	local mode = tostring(change.mode or "")
	local sessionKey = reconcileSessionKey(serviceName, change.reconcileSession)
	pruneExpiredSessions(reconcileSessions)
	if mode == "beginReconcileService" then
		if reconcileSessions[sessionKey] == nil and countEntries(reconcileSessions) >= MAX_RECONCILE_SESSIONS then
			error("Too many active editor reconcile sessions")
		end
		reconcileSessions[sessionKey] = {
			serviceName = serviceName,
			desiredKeys = {},
			desiredSettingsIds = {},
			desiredStableKeys = {},
			resolvedEntries = {},
			claimedInstances = {},
			failed = false,
			entryCount = 0,
			updatedAt = os.clock(),
		}
		clearMatchedSettingsInstances(serviceName, ctx)
	elseif reconcileSessions[sessionKey] == nil then
		error("Editor reconcile session was not found or expired; restart the reconcile")
	end
	local session = reconcileSessions[sessionKey]
	armSessionExpiry(reconcileSessions, sessionKey, session)
	if session.failed then
		if mode == "finishReconcileService" then
			reconcileSessions[sessionKey] = nil
		end
		error("Skipping reconcile chunk after an earlier chunk failed")
	end

	local beforeCreated = stats.instanceCreated
	local beforeDeleted = stats.instanceDeleted
	local beforeReplaced = stats.instanceReplaced
	local entries = sortedInstanceEntries(change, service.Name)
	local newEntries = 0
	for _, entry in ipairs(entries) do
		if not session.desiredKeys[entry.key] then
			newEntries += 1
		end
	end
	if session.entryCount + newEntries > MAX_RECONCILE_ENTRIES then
		session.failed = true
		error("Editor reconcile session exceeds the supported instance count")
	end
	session.entryCount += newEntries
	for _, entry in ipairs(entries) do
		session.desiredKeys[entry.key] = true
		if #entry.pathSegments > 1 then
			if isProtectedWorkspaceCameraPath(entry.pathSegments) then
				stats.noops += 1
			else
				local instance =
					resolveEntryInstance(entry, service.Name, ctx, session.resolvedEntries, session.claimedInstances)
				if instance == nil then
					local parent = resolveEntryParent(entry, session.resolvedEntries)
					if parent == nil then
						error("Cannot create instance; parent path was not found: " .. entry.key)
					end
					local okCreate, created = pcall(Instance.new, entry.className)
					if not okCreate or created == nil then
						error(`Cannot create {entry.className} at {pathKey(entry.pathSegments)}: {created}`)
					end
					created.Name = tostring(entry.pathSegments[#entry.pathSegments])
					created.Parent = parent
					if created:IsA("MeshPart") then
						recentlyCreatedMeshParts[created] = true
					end
					instance = created
					stats.instanceCreated += 1
				else
					syncEntryPlacement(entry, instance, stats, session.resolvedEntries)
					if instance.ClassName ~= entry.className then
						local oldInstance = instance
						instance = replaceInstanceClass(instance, entry.className, stats, ctx.selectionReplacements)
						rememberReplacementIdentity(service.Name, entry.settingsId, oldInstance, instance, ctx)
					end
				end
				session.resolvedEntries[entry.key] = instance
				rememberEntryResolution(entry, service.Name, instance, session.claimedInstances, ctx)
				recordDesiredStableEntry(
					entry,
					service.Name,
					instance,
					ctx,
					session.desiredSettingsIds,
					session.desiredStableKeys
				)
			end
		end
	end

	if mode == "finishReconcileService" then
		if not keepUnknownsEnabled(ctx) then
			local descendants = service:GetDescendants()
			for i = #descendants, 1, -1 do
				local instance = descendants[i]
				local pathSegments = getInstancePathSegments(instance)
				local pathOrdinals = getInstancePathOrdinals(instance)
				local key = pathSegments and pathCacheKey(pathSegments, pathOrdinals) or ""
				if key ~= "" and not shouldKeepInstanceByDesiredEntry(service.Name, instance, pathSegments, pathOrdinals, ctx, session.desiredKeys, session.desiredSettingsIds, session.desiredStableKeys) and not isProtectedWorkspaceCameraInstance(instance) then
					removeInstanceForUndo(instance)
					stats.instanceDeleted += 1
				end
			end
		end
		reconcileSessions[sessionKey] = nil
	end

	if stats.instanceCreated == beforeCreated and stats.instanceDeleted == beforeDeleted and stats.instanceReplaced == beforeReplaced then
		stats.noops += 1
	end
end

local function applyInstanceUpserts(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true

	local beforeCreated = stats.instanceCreated
	local beforeReplaced = stats.instanceReplaced
	local resolvedEntries = {}
	local claimedInstances = {}
	for _, entry in ipairs(sortedInstanceEntries(change, service.Name)) do
		if #entry.pathSegments > 1 then
			if isProtectedWorkspaceCameraPath(entry.pathSegments) then
				stats.noops += 1
			else
				local instance = resolveEntryInstance(entry, service.Name, ctx, resolvedEntries, claimedInstances)
				if instance == nil and not liveHydrateEnabled(ctx) then
					stats.noops += 1
				elseif instance == nil then
					local parent = resolveEntryParent(entry, resolvedEntries)
					if parent == nil then
						error("Cannot create instance; parent path was not found: " .. entry.key)
					end
					local okCreate, created = pcall(Instance.new, entry.className)
					if not okCreate or created == nil then
						error(`Cannot create instance {entry.key}: {created}`)
					end
					created.Name = tostring(entry.pathSegments[#entry.pathSegments])
					created.Parent = parent
					if created:IsA("MeshPart") then
						recentlyCreatedMeshParts[created] = true
					end
					instance = created
					stats.instanceCreated += 1
				else
					syncEntryPlacement(entry, instance, stats, resolvedEntries)
					if instance.ClassName ~= entry.className then
						local oldInstance = instance
						instance = replaceInstanceClass(instance, entry.className, stats, ctx.selectionReplacements)
						rememberReplacementIdentity(service.Name, entry.settingsId, oldInstance, instance, ctx)
					end
				end
				if instance ~= nil then
					resolvedEntries[entry.key] = instance
					rememberEntryResolution(entry, service.Name, instance, claimedInstances, ctx)
				end
			end
		end
	end

	if stats.instanceCreated == beforeCreated and stats.instanceReplaced == beforeReplaced then
		stats.noops += 1
	end
end

local function applyInstanceDeletes(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true

	local beforeDeleted = stats.instanceDeleted
	local targets = {}
	local seenTargets = {}
	for _, entry in ipairs(sortedInstanceEntries(change, service.Name)) do
		if #entry.pathSegments <= 1 then
			error("Refusing to delete service root: " .. entry.key)
		end
		local instance = resolveInstanceBySettingsId(service.Name, entry.settingsId, ctx)
		if instance ~= nil and not instanceMatchesExpectedClass(instance, entry.className) then
			instance = nil
		end
		if instance == nil then
			instance = resolvePathSegments(entry.pathSegments, nil, entry.pathOrdinals)
			if instance ~= nil and not instanceMatchesExpectedClass(instance, entry.className) then
				instance = nil
			end
		end
		if instance == nil or isProtectedWorkspaceCameraInstance(instance) or seenTargets[instance] then
			stats.noops += 1
		else
			seenTargets[instance] = true
			targets[#targets + 1] = instance
		end
	end
	for _, instance in ipairs(targets) do
		removeInstanceForUndo(instance)
		stats.instanceDeleted += 1
	end

	if stats.instanceDeleted == beforeDeleted then
		stats.noops += 1
	end
end

local function applyInstanceChange(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local mode = tostring(change.mode or "reconcileService")
	if mode == "reconcileService" then
		applyInstanceReconcile(change, ctx, stats, touchedServices)
	elseif mode == "beginReconcileService" or mode == "reconcileServiceChunk" or mode == "finishReconcileService" then
		applyInstanceReconcileChunk(change, ctx, stats, touchedServices)
	elseif mode == "upsertInstances" or mode == "replaceInstances" then
		applyInstanceUpserts(change, ctx, stats, touchedServices)
	elseif mode == "deleteInstances" then
		applyInstanceDeletes(change, ctx, stats, touchedServices)
	else
		error("Unsupported instance sync mode: " .. mode)
	end
end

local function recordProtectedWrite(stats, change, kind, name, value, deleted)
	stats.protectedSkipped += 1
	local row = {
		kind = kind,
		service = change.service,
		settingsId = change.settingsId,
		pathSegments = change.pathSegments,
		pathOrdinals = change.pathOrdinals,
		className = change.className,
		name = name,
	}
	if value ~= nil then
		row.value = value
	end
	if deleted then
		row.deleted = true
	end
	table.insert(stats.protectedWrites, row)
end

local function applyPropertyChange(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true
	if tostring(change.className or "") == "PackageLink" then
		stats.noops += 1
		return
	end

	local instance = resolveInstance(change, ctx)
	if instance == nil then
		error(`Target instance was not found: {pathKey(cloneArray(change.pathSegments))} [{change.className or ""}]`)
	end
	assertInstanceInService(instance, service)
	if isProtectedWorkspaceCameraPath(change.pathSegments) or isProtectedWorkspaceCameraInstance(instance) then
		stats.noops += 1
		return
	end

	local properties = change.properties
	if type(properties) == "table" then
		local propertyNames = {}
		if properties.MeshId ~= nil then
			table.insert(propertyNames, "MeshId")
		end
		for propertyName in pairs(properties) do
			propertyName = tostring(propertyName)
			if propertyName ~= "MeshId" then
				table.insert(propertyNames, propertyName)
			end
		end
		for _, propertyName in ipairs(propertyNames) do
			local rawValue = properties[propertyName]
			if propertyName == "Source" then
				stats.noops += 1
			elseif propertyName == "ClassName" then
				if tostring(rawValue) == instance.ClassName then
					stats.noops += 1
				else
					error("ClassName changes are not supported for " .. instance:GetFullName())
				end
			elseif propertyName == "Name" then
				local nextName = tostring(rawValue)
				if instance.Name == nextName then
					stats.noops += 1
				else
					instance.Name = nextName
					stats.propertyUpdated += 1
				end
			elseif propertyName == "Tags" then
				applyTags(instance, rawValue, stats)
			else
				if not classHasProperty(instance, propertyName) then
					stats.noops += 1
					continue
				end
				local okDecode, decoded = decodePropertyValue(instance, propertyName, rawValue, ctx, serviceName)
				if not okDecode then
					error(`Failed to decode {propertyName}: {decoded}`)
				end
				local okRead, current = readProperty(instance, propertyName)
				if okRead and valuesEqual(current, decoded) then
					stats.noops += 1
					if propertyName == "MeshId" then
						clearRecentlyCreatedMeshPart(instance)
					end
				else
					if propertyName == "MeshId" and instance:IsA("MeshPart") and not canApplyProtectedMeshId(change, instance) then
						recordProtectedWrite(stats, change, "property", propertyName, rawValue)
						stats.noops += 1
						clearRecentlyCreatedMeshPart(instance)
						continue
					end
					local okWrite, err = writeProperty(instance, propertyName, decoded)
					if not okWrite then
						if propertyName == "MeshId" and instance:IsA("MeshPart") then
							local okApplyMesh, applyMeshErr = applyMeshPartMeshId(instance, decoded)
							if not okApplyMesh then
								error(`Failed to apply MeshId on {instance:GetFullName()}: {applyMeshErr}`)
							end
						else
							local errText = string.lower(tostring(err))
							if string.find(errText, "read only", 1, true) or string.find(errText, "lacking capability robloxscript", 1, true) or string.find(errText, "not a valid member", 1, true) then
								recordProtectedWrite(stats, change, "property", propertyName, rawValue)
								stats.noops += 1
								continue
							end
							error(`Failed to write {propertyName} on {instance:GetFullName()}: {err}`)
						end
					end
					stats.propertyUpdated += 1
					if propertyName == "MeshId" then
						clearRecentlyCreatedMeshPart(instance)
					end
				end
			end
		end
	end

	local deletedAttributes = change.deletedAttributes
	if type(deletedAttributes) == "table" then
		for _, attributeName in ipairs(deletedAttributes) do
			local current = instance:GetAttribute(attributeName)
			if current == nil then
				stats.noops += 1
			else
				local okWrite, err = pcall(instance.SetAttribute, instance, attributeName, nil)
				if not okWrite then
					local errText = string.lower(tostring(err))
					if string.find(errText, "corescript permission required", 1, true) or string.find(errText, "read only", 1, true) then
						recordProtectedWrite(stats, change, "attribute", attributeName, nil, true)
						stats.noops += 1
						continue
					end
					error(`Failed to delete attribute {attributeName} on {instance:GetFullName()}: {err}`)
				end
				stats.attributeUpdated += 1
			end
		end
	end

	local attributes = change.attributes
	if type(attributes) == "table" then
		for attributeName, rawValue in pairs(attributes) do
			attributeName = tostring(attributeName)
			local okDecode, decoded = decodeValue(rawValue, nil)
			if not okDecode then
				error(`Failed to decode attribute {attributeName}: {decoded}`)
			end
			local current = instance:GetAttribute(attributeName)
			if valuesEqual(current, decoded) then
				stats.noops += 1
			else
				local okWrite, err = pcall(instance.SetAttribute, instance, attributeName, decoded)
				if not okWrite then
					local errText = string.lower(tostring(err))
					if string.find(errText, "corescript permission required", 1, true) or string.find(errText, "read only", 1, true) then
						recordProtectedWrite(stats, change, "attribute", attributeName, rawValue)
						stats.noops += 1
						continue
					end
					error(`Failed to write attribute {attributeName} on {instance:GetFullName()}: {err}`)
				end
				stats.attributeUpdated += 1
			end
		end
	end
end

local function validateObjectTable(raw: any, label: string)
	if type(raw) ~= "table" then
		error(label .. " must be an object")
	end
	for key in pairs(raw) do
		if type(key) ~= "string" or key == "" then
			error(label .. " must use non-empty string keys")
		end
	end
end

local function validateMutationPath(change: { [string]: any }, serviceName: string, label: string)
	local pathIsArray, pathLength = denseArrayLength(change.pathSegments)
	if not pathIsArray or pathLength == 0 then
		error(label .. " pathSegments must be a non-empty array")
	end
	for index, segment in ipairs(change.pathSegments) do
		if type(segment) ~= "string" or segment == "" then
			error(string.format("%s path segment %d must be a non-empty string", label, index))
		end
	end
	if change.pathSegments[1] ~= serviceName then
		error(label .. " path root does not match its service")
	end
	if change.pathOrdinals ~= nil then
		local ordinalsAreArray, ordinalCount = denseArrayLength(change.pathOrdinals)
		if not ordinalsAreArray or ordinalCount > pathLength then
			error(label .. " pathOrdinals must be a path-sized array")
		end
		for index, ordinal in ipairs(change.pathOrdinals) do
			if type(ordinal) ~= "number" or ordinal < 1 or ordinal % 1 ~= 0 then
				error(string.format("%s path ordinal %d must be a positive integer", label, index))
			end
		end
	end
end

local function validateCreatableClass(className: any, cache: { [string]: boolean }, label: string): string
	if type(className) ~= "string" or className == "" then
		error(label .. " className must be a non-empty string")
	end
	if not cache[className] then
		local okCreate, instance = pcall(Instance.new, className)
		if not okCreate or instance == nil then
			error(`{label} className is not creatable: {className}`)
		end
		instance:Destroy()
		cache[className] = true
	end
	return className
end

local function validateMutationRequest(params: any, ctx: { [string]: any }): { string }
	if type(params) ~= "table" then
		error("Editor mutation request must be an object")
	end
	if params.probeEvents ~= nil and type(params.probeEvents) ~= "boolean" then
		error("Editor mutation probeEvents must be a boolean")
	end
	local serviceSet = {}
	local classCache = {}
	local maxChanges = tonumber(ctx.maxChangesPerRequest) or 5000
	local function validateList(rawChanges: any, kind: string)
		if rawChanges == nil then
			return
		end
		local changesAreArray, changeCount = denseArrayLength(rawChanges)
		if not changesAreArray then
			error(`Editor {kind} changes must be an array`)
		end
		if changeCount > maxChanges then
			error(`Editor mutation request has too many {kind} changes`)
		end
		for changeIndex, change in ipairs(rawChanges) do
			if type(change) ~= "table" then
				error(string.format("Editor %s change %d must be an object", kind, changeIndex))
			end
			if type(change.service) ~= "string" or not ctx.allowedServices[change.service] then
				error(string.format("Editor %s change %d has an invalid service", kind, changeIndex))
			end
			local serviceName = change.service
			serviceSet[serviceName] = true
			if kind ~= "instance" then
				validateMutationPath(change, serviceName, string.format("Editor %s change %d", kind, changeIndex))
			end
			if kind == "instance" then
				local mode = change.mode
				if
					mode ~= "reconcileService"
					and mode ~= "beginReconcileService"
					and mode ~= "reconcileServiceChunk"
					and mode ~= "finishReconcileService"
					and mode ~= "upsertInstances"
					and mode ~= "replaceInstances"
					and mode ~= "deleteInstances"
				then
					error("Editor instance change has an unsupported mode")
				end
				if change.allowDeletes ~= nil and type(change.allowDeletes) ~= "boolean" then
					error("Editor instance allowDeletes must be a boolean")
				end
				if
					(mode == "beginReconcileService" or mode == "reconcileServiceChunk" or mode == "finishReconcileService")
					and (type(change.reconcileSession) ~= "string" or change.reconcileSession == "")
				then
					error("Editor chunked reconcile requires a session id")
				end
				local instancesAreArray, instanceCount = denseArrayLength(change.instances)
				if not instancesAreArray or instanceCount > (tonumber(ctx.maxInstanceEntriesPerChange) or 5000) then
					error("Editor instance entries must be a bounded array")
				end
				for entryIndex, entry in ipairs(change.instances) do
					if type(entry) ~= "table" then
						error(string.format("Editor instance entry %d must be an object", entryIndex))
					end
					validateMutationPath(
						{
							pathSegments = entry.pathSegments,
							pathOrdinals = entry.pathOrdinals,
						},
						serviceName,
						string.format("Editor instance entry %d", entryIndex)
					)
					validateCreatableClass(entry.className, classCache, "Editor instance entry")
					if entry.matchProperties ~= nil then
						validateObjectTable(entry.matchProperties, "Editor instance matchProperties")
					end
					if entry.matchAttributes ~= nil then
						validateObjectTable(entry.matchAttributes, "Editor instance matchAttributes")
					end
				end
			elseif kind == "source" then
				local className = validateCreatableClass(change.className, classCache, "Editor source change")
				if not ctx.luaSourceClass[className] then
					error("Editor source class is not a Lua source container")
				end
				if change.deleted ~= nil and type(change.deleted) ~= "boolean" then
					error("Editor source deleted must be a boolean")
				end
				if change.deleted ~= true and type(change.source) ~= "string" then
					error("Editor source must be a string")
				end
				if type(change.source) == "string" and #change.source > (tonumber(ctx.maxSourceBytes) or 8 * 1024 * 1024) then
					error("Editor source mutation exceeds safe size limit")
				end
			elseif kind == "property" then
				if type(change.className) ~= "string" or change.className == "" then
					error("Editor property className must be a non-empty string")
				end
				if change.properties ~= nil then
					validateObjectTable(change.properties, "Editor properties")
				end
				if change.attributes ~= nil then
					validateObjectTable(change.attributes, "Editor attributes")
					for attributeName, rawValue in pairs(change.attributes) do
						local okDecode, decoded = decodeValue(rawValue, nil, ctx, serviceName)
						if not okDecode then
							error(`Editor attribute {attributeName} is invalid: {decoded}`)
						end
						if decoded ~= nil and typeof(decoded) == "table" then
							error(`Editor attribute {attributeName} has an unsupported value`)
						end
					end
				end
				if change.deletedAttributes ~= nil then
					local deletionsAreArray = denseArrayLength(change.deletedAttributes)
					if not deletionsAreArray then
						error("Editor deletedAttributes must be an array")
					end
					local seenDeletedAttributes = {}
					for index, attributeName in ipairs(change.deletedAttributes) do
						if type(attributeName) ~= "string" or attributeName == "" then
							error(string.format("Editor deleted attribute %d must be a non-empty string", index))
						end
						if seenDeletedAttributes[attributeName] then
							error("Editor deletedAttributes must not contain duplicates")
						end
						if type(change.attributes) == "table" and change.attributes[attributeName] ~= nil then
							error("Editor attribute cannot be updated and deleted in one change")
						end
						seenDeletedAttributes[attributeName] = true
					end
				end
			end
		end
	end
	validateList(params.instanceChanges, "instance")
	validateList(params.sourceChanges, "source")
	validateList(params.propertyChanges, "property")
	local services = {}
	for serviceName in pairs(serviceSet) do
		table.insert(services, serviceName)
	end
	table.sort(services)
	return services
end

local function addSnapshotMetadataTarget(targets: { any }, seen: { [Instance]: boolean }, instance: Instance?)
	if instance ~= nil and not seen[instance] then
		seen[instance] = true
		table.insert(targets, instance)
	end
end

local function mutationSnapshotLayout(serviceNames: { string })
	local groups = {}
	local roots = {}
	local metadataTargets = {}
	local metadataSeen = {}
	for _, serviceName in ipairs(serviceNames) do
		local service = game:GetService(serviceName)
		local preserved = {}
		addSnapshotMetadataTarget(metadataTargets, metadataSeen, service)
		if service == Workspace then
			local terrain = Workspace:FindFirstChildOfClass("Terrain")
			if terrain ~= nil then
				preserved[terrain] = true
				addSnapshotMetadataTarget(metadataTargets, metadataSeen, terrain)
			end
			local currentCamera = Workspace.CurrentCamera
			if currentCamera ~= nil then
				preserved[currentCamera] = true
				addSnapshotMetadataTarget(metadataTargets, metadataSeen, currentCamera)
			end
		end
		if serviceName == "StarterPlayer" then
			for _, className in ipairs({ "StarterPlayerScripts", "StarterCharacterScripts" }) do
				local container = service:FindFirstChildOfClass(className)
				if container ~= nil then
					preserved[container] = true
					addSnapshotMetadataTarget(metadataTargets, metadataSeen, container)
					local children = container:GetChildren()
					table.insert(groups, {
						serviceName = serviceName,
						target = container,
						count = #children,
						preserved = {},
					})
					for _, child in ipairs(children) do
						table.insert(roots, child)
					end
				end
			end
		end
		local children = {}
		for _, child in ipairs(service:GetChildren()) do
			if not preserved[child] then
				table.insert(children, child)
			end
		end
		table.insert(groups, {
			serviceName = serviceName,
			target = service,
			count = #children,
			preserved = preserved,
		})
		for _, child in ipairs(children) do
			table.insert(roots, child)
		end
	end
	return groups, roots, metadataTargets, metadataSeen
end

local function captureMutationSnapshot(serviceNames: { string }, params: { [string]: any }, ctx: { [string]: any })
	local groups, roots, metadataTargets, metadataSeen = mutationSnapshotLayout(serviceNames)
	local payload = nil
	if #roots > 0 then
		local okSerialize, serialized = pcall(SerializationService.SerializeInstancesAsync, SerializationService, roots)
		if not okSerialize then
			error("Cannot create an editor rollback snapshot: " .. tostring(serialized))
		end
		payload = serialized
	end
	local metadata = table.create(#metadataTargets)
	for index, instance in ipairs(metadataTargets) do
		metadata[index] = {
			instance = instance,
			attributes = instance:GetAttributes(),
			tags = CollectionService:GetTags(instance),
		}
	end
	local properties = {}
	local propertySeen = {}
	for _, change in ipairs(params.propertyChanges or {}) do
		local instance = resolveInstance(change, ctx, true)
		if instance ~= nil and metadataSeen[instance] then
			local seenNames = propertySeen[instance]
			if seenNames == nil then
				seenNames = {}
				propertySeen[instance] = seenNames
			end
			for propertyName in pairs(change.properties or {}) do
				propertyName = tostring(propertyName)
				if propertyName ~= "Tags" and not seenNames[propertyName] then
					local okRead, value = readProperty(instance, propertyName)
					if not okRead then
						error(`Cannot snapshot {instance:GetFullName()}.{propertyName}`)
					end
					seenNames[propertyName] = true
					table.insert(properties, {
						instance = instance,
						name = propertyName,
						value = value,
					})
				end
			end
		end
	end
	local originalByPath = {}
	local originalRoots = {}
	for _, group in ipairs(groups) do
		for _, child in ipairs(group.target:GetChildren()) do
			if not group.preserved[child] then
				table.insert(originalRoots, child)
				local instances = { child }
				for _, descendant in ipairs(child:GetDescendants()) do
					table.insert(instances, descendant)
				end
				for _, instance in ipairs(instances) do
					local pathSegments = getInstancePathSegments(instance)
					if pathSegments ~= nil then
						originalByPath[pathCacheKey(pathSegments, getInstancePathOrdinals(instance))] = instance
					end
				end
			end
		end
	end
	return {
		groups = groups,
		payload = payload,
		rootCount = #roots,
		metadata = metadata,
		properties = properties,
		originalByPath = originalByPath,
		originalRoots = originalRoots,
		currentCamera = Workspace.CurrentCamera,
	}
end

local function restoreSnapshotMetadata(snapshot: { [string]: any }, replacements: { [Instance]: Instance })
	for _, entry in ipairs(snapshot.metadata) do
		local instance = replacements[entry.instance] or entry.instance
		local desiredAttributes = entry.attributes
		for name in pairs(instance:GetAttributes()) do
			if desiredAttributes[name] == nil then
				instance:SetAttribute(name, nil)
			end
		end
		for name, value in pairs(desiredAttributes) do
			instance:SetAttribute(name, value)
		end
		local desiredTags = {}
		for _, tag in ipairs(entry.tags) do
			desiredTags[tag] = true
		end
		for _, tag in ipairs(CollectionService:GetTags(instance)) do
			if not desiredTags[tag] then
				CollectionService:RemoveTag(instance, tag)
			end
		end
		for tag in pairs(desiredTags) do
			if not CollectionService:HasTag(instance, tag) then
				CollectionService:AddTag(instance, tag)
			end
		end
	end
	for _, entry in ipairs(snapshot.properties) do
		local instance = replacements[entry.instance] or entry.instance
		local value = if typeof(entry.value) == "Instance" then replacements[entry.value] or entry.value else entry.value
		local okWrite, writeError = writeProperty(instance, entry.name, value)
		if not okWrite then
			error(`Could not restore {instance:GetFullName()}.{entry.name}: {writeError}`)
		end
	end
end

local function restoreMutationSnapshot(
	snapshot: { [string]: any },
	ctx: { [string]: any },
	mutationReplacements: { [Instance]: Instance }?
): { [Instance]: Instance }
	local roots = {}
	if snapshot.payload ~= nil then
		roots = SerializationService:DeserializeInstancesAsync(snapshot.payload)
	end
	if #roots ~= snapshot.rootCount then
		error("Editor rollback snapshot returned an unexpected root count")
	end
	local incomingByGroup = {}
	local rootIndex = 1
	for groupIndex, group in ipairs(snapshot.groups) do
		local incoming = table.create(group.count)
		for index = 1, group.count do
			local instance = roots[rootIndex]
			rootIndex += 1
			if instance == nil or instance.Parent ~= nil then
				error("Editor rollback snapshot returned an invalid root")
			end
			incoming[index] = instance
		end
		incomingByGroup[groupIndex] = incoming
	end
	local removed = {}
	local parented = {}
	local okRestore, restoreError = pcall(function()
		for groupIndex, group in ipairs(snapshot.groups) do
			for _, child in ipairs(group.target:GetChildren()) do
				if not group.preserved[child] then
					child.Parent = nil
					table.insert(removed, { instance = child, parent = group.target })
				end
			end
			for _, instance in ipairs(incomingByGroup[groupIndex]) do
				instance.Parent = group.target
				table.insert(parented, instance)
			end
		end
	end)
	if not okRestore then
		for _, instance in ipairs(parented) do
			instance.Parent = nil
		end
		for _, entry in ipairs(removed) do
			entry.instance.Parent = entry.parent
		end
		error(restoreError, 0)
	end
	local replacements = {}
	for key, original in pairs(snapshot.originalByPath) do
		local separator = string.find(key, PATH_SEPARATOR .. "ord" .. PATH_SEPARATOR, 1, true)
		local pathText = if separator ~= nil then string.sub(key, 1, separator - 1) else key
		local ordinalText = if separator ~= nil then string.sub(key, separator + 5) else ""
		local pathSegments = string.split(pathText, PATH_SEPARATOR)
		local pathOrdinals = {}
		if ordinalText ~= "" then
			for index, value in ipairs(string.split(ordinalText, ",")) do
				pathOrdinals[index] = tonumber(value)
			end
		end
		local replacement = resolvePathSegments(pathSegments, nil, pathOrdinals)
		if replacement ~= nil then
			replacements[original] = replacement
		end
	end
	if mutationReplacements ~= nil then
		for original, mutationReplacement in pairs(mutationReplacements) do
			local restored = replacements[original]
			if restored ~= nil then
				replacements[mutationReplacement] = restored
			end
		end
	end
	if next(replacements) ~= nil then
		local scanRoots = {}
		for serviceName, allowed in pairs(ctx.allowedServices) do
			if allowed then
				table.insert(scanRoots, game:GetService(serviceName))
			end
		end
		local updated, failed = BridgeReferenceRetarget.apply(
			scanRoots,
			replacements,
			RbxDomModule.getReferencePropertyNames,
			readProperty,
			writeProperty
		)
		if failed > 0 then
			error(string.format("Could not restore %d external instance references", failed))
		end
	end
	restoreSnapshotMetadata(snapshot, replacements)
	if snapshot.currentCamera ~= nil then
		Workspace.CurrentCamera = replacements[snapshot.currentCamera] or snapshot.currentCamera
	end
	local destroyed = {}
	for _, root in ipairs(removed) do
		destroyed[root.instance] = true
		root.instance:Destroy()
	end
	for _, root in ipairs(snapshot.originalRoots) do
		if root.Parent == nil and not destroyed[root] then
			root:Destroy()
		end
	end
	return replacements
end

function BridgeEditorSync.create(ctx: { [string]: any })
	local api = {}
	api.stats = ctx.stats

	local function rollbackReconcileSnapshot(snapshot: { [string]: any }, serviceName: string)
		local selected = captureExplorerSelection()
		local recording = beginHistoryRecording("Cancel filesystem reconcile")
		local okRestore, replacements = pcall(restoreMutationSnapshot, snapshot, ctx, nil)
		finishHistoryRecording(recording, Enum.FinishRecordingOperation.Cancel)
		if not okRestore then
			error("Could not roll back the editor reconcile: " .. tostring(replacements))
		end
		restoreExplorerSelection(selected, replacements)
		ctx.invalidateService(serviceName)
	end

	function api.resolveReviewInstance(change: { [string]: any }): Instance?
		return resolveInstance(change, ctx)
	end

	function api.decodeReviewValue(raw: any, enumHint: string?, serviceName: string?): (boolean, any)
		return decodeValue(raw, enumHint, ctx, serviceName)
	end

	function api.readReviewProperty(instance: Instance, propertyName: string): (boolean, any)
		return readProperty(instance, propertyName)
	end

	function api.beginBinaryExport(params: { [string]: any }): { [string]: any }
		pruneExpiredSessions(binaryExports)
		local exportId = tostring(params.exportId or "")
		if exportId == "" then
			error("Invalid native export id")
		end
		for key in pairs(binaryExports) do
			binaryExports[key] = nil
		end
		local serviceNames = {}
		for serviceName, allowed in pairs(ctx.allowedServices) do
			if allowed then
				serviceNames[#serviceNames + 1] = serviceName
			end
		end
		table.sort(serviceNames)
		local roots = {}
		local markers = {}
		local groups = {}
		for _, serviceName in ipairs(serviceNames) do
			local service = game:GetService(serviceName)
			local marker = Instance.new("Folder")
			marker.Name = serviceName
			for name, value in pairs(service:GetAttributes()) do
				pcall(marker.SetAttribute, marker, name, value)
			end
			for _, tag in ipairs(CollectionService:GetTags(service)) do
				CollectionService:AddTag(marker, tag)
			end
			markers[#markers + 1] = marker
			roots[#roots + 1] = marker
			local children = service:GetChildren()
			local rootProperties = {}
			if type(ctx.readRootProperties) == "function" then
				local values = ctx.readRootProperties(serviceName)
				if type(values) == "table" then
					rootProperties = values
				end
			end
			groups[#groups + 1] = {
				service = serviceName,
				targetPath = { serviceName },
				count = #children,
				rootProperties = rootProperties,
			}
			for _, child in ipairs(children) do
				roots[#roots + 1] = child
			end
		end
		local ok, payload = pcall(SerializationService.SerializeInstancesAsync, SerializationService, roots)
		for _, marker in ipairs(markers) do
			marker:Destroy()
		end
		if not ok then
			error(payload, 0)
		end
		local totalBytes = buffer.len(payload)
		if totalBytes > 536870912 then
			error("Native export exceeds the supported size")
		end
		binaryExports[exportId] = {
			payload = payload,
			groups = groups,
			totalBytes = totalBytes,
			updatedAt = os.clock(),
		}
		armSessionExpiry(binaryExports, exportId, binaryExports[exportId])
		return {
			ok = true,
			exportId = exportId,
			totalBytes = totalBytes,
			groups = groups,
		}
	end

	function api.readBinaryExport(params: { [string]: any }): { [string]: any }
		pruneExpiredSessions(binaryExports)
		local exportId = tostring(params.exportId or "")
		local session = binaryExports[exportId]
		if type(session) ~= "table" then
			error("Native export session was not found")
		end
		armSessionExpiry(binaryExports, exportId, session)
		local offset = tonumber(params.offset)
		local length = tonumber(params.length)
		if offset == nil or offset < 0 or offset % 1 ~= 0 then
			error("Invalid native export offset")
		end
		if length == nil or length < 1 or length > 4194304 or length % 1 ~= 0 then
			error("Invalid native export length")
		end
		if offset + length > session.totalBytes then
			error("Native export range exceeds its payload")
		end
		local chunk = buffer.create(length)
		buffer.copy(chunk, 0, session.payload, offset, length)
		return {
			ok = true,
			offset = offset,
			length = length,
			data = buffer.tostring(EncodingService:Base64Encode(chunk)),
		}
	end

	function api.finishBinaryExport(params: { [string]: any }): { [string]: any }
		local exportId = tostring(params.exportId or "")
		local found = binaryExports[exportId] ~= nil
		binaryExports[exportId] = nil
		return { ok = true, found = found }
	end

	function api.beginBinaryImport(params: { [string]: any }): { [string]: any }
		pruneExpiredSessions(binaryImports)
		pruneCompletedBinaryImports()
		local importId = tostring(params.importId or "")
		local totalBytes = tonumber(params.totalBytes)
		local totalChunks = tonumber(params.totalChunks)
		if
			importId == ""
			or totalBytes == nil
			or totalBytes < 1
			or totalBytes > 536870912
			or totalBytes % 1 ~= 0
		then
			error("Invalid native import size")
		end
		local expectedChunks = math.ceil(totalBytes / BINARY_IMPORT_CHUNK_BYTES)
		if
			totalChunks == nil
			or totalChunks ~= expectedChunks
			or totalChunks < 1
			or totalChunks > 4096
			or totalChunks % 1 ~= 0
		then
			error("Invalid native import chunk count")
		end
		local groupsAreArray, groupCount = denseArrayLength(params.groups)
		if not groupsAreArray or groupCount == 0 then
			error("Native import groups must be an array")
		end
		local instanceCount = tonumber(params.instanceCount)
		if
			not instanceCount
			or instanceCount ~= instanceCount
			or instanceCount < 0
			or instanceCount % 1 ~= 0
		then
			error("Invalid native import instance count")
		end
		if completedBinaryImports[importId] ~= nil then
			error("Native import id was already completed")
		end
		if binaryImports[importId] == nil and countEntries(binaryImports) >= MAX_BINARY_IMPORT_SESSIONS then
			error("Too many active native import sessions")
		end
		local bufferedBytes = totalBytes
		for activeId, active in pairs(binaryImports) do
			if activeId ~= importId then
				bufferedBytes += tonumber(active.totalBytes) or 0
			end
		end
		if bufferedBytes > MAX_BINARY_IMPORT_BUFFERED_BYTES then
			error("Native import sessions exceed the aggregate buffered-byte limit")
		end
		local groups = {}
		for _, rawGroup in ipairs(params.groups) do
			validateObjectTable(rawGroup, "Native import group")
			local serviceName, service = validatedChangeService({ service = rawGroup.service }, ctx)
			local targetPath = rawGroup.targetPath
			local targetPathIsArray, targetPathLength = denseArrayLength(targetPath)
			if
				not targetPathIsArray
				or targetPathLength < 1
				or targetPathLength > 2
				or type(targetPath[1]) ~= "string"
				or targetPath[1] ~= serviceName
			then
				error("Invalid native import target path")
			end
			local target = service
			if targetPathLength == 2 then
				if type(targetPath[2]) ~= "string" or targetPath[2] == "" then
					error("Invalid native import nested target")
				end
				target = service:FindFirstChild(targetPath[2])
				if target == nil then
					error("Native import target was not found")
				end
			end
			local count = tonumber(rawGroup.count)
			if count == nil or count < 0 or count % 1 ~= 0 then
				error("Invalid native import service count")
			end
			groups[#groups + 1] = {
				serviceName = serviceName,
				service = service,
				target = target,
				count = count,
			}
		end
		binaryImports[importId] = {
			totalBytes = totalBytes,
			totalChunks = totalChunks,
			payload = buffer.create(totalBytes),
			received = table.create(totalChunks),
			receivedBytes = 0,
			receivedChunks = 0,
			instanceCount = instanceCount,
			groups = groups,
			updatedAt = os.clock(),
		}
		armSessionExpiry(binaryImports, importId, binaryImports[importId])
		return { ok = true, importId = importId }
	end

	function api.appendBinaryImport(params: { [string]: any }): { [string]: any }
		pruneExpiredSessions(binaryImports)
		local importId = tostring(params.importId or "")
		local session = binaryImports[importId]
		if type(session) ~= "table" then
			error("Native import session was not found")
		end
		armSessionExpiry(binaryImports, importId, session)
		local index = tonumber(params.index)
		if index == nil or index < 1 or index > session.totalChunks or index % 1 ~= 0 then
			error("Invalid native import chunk index")
		end
		if session.received[index] then
			return { ok = true, duplicate = true }
		end
		local data = tostring(params.data or "")
		local decoded = EncodingService:Base64Decode(buffer.fromstring(data))
		local decodedBytes = buffer.len(decoded)
		local offset = (index - 1) * BINARY_IMPORT_CHUNK_BYTES
		local expectedBytes = math.min(BINARY_IMPORT_CHUNK_BYTES, session.totalBytes - offset)
		if decodedBytes ~= expectedBytes then
			error("Native import chunk has the wrong decoded size")
		end
		buffer.copy(session.payload, offset, decoded, 0, decodedBytes)
		session.received[index] = true
		session.receivedBytes += decodedBytes
		session.receivedChunks += 1
		return { ok = true, receivedBytes = decodedBytes }
	end

	function api.finishBinaryImport(params: { [string]: any }): { [string]: any }
		pruneExpiredSessions(binaryImports)
		pruneCompletedBinaryImports()
		local importId = tostring(params.importId or "")
		local completed = completedBinaryImports[importId]
		if type(completed) == "table" then
			return completed.response
		end
		local session = binaryImports[importId]
		if type(session) ~= "table" then
			error("Native import session was not found")
		end
		if session.receivedChunks ~= session.totalChunks or session.receivedBytes ~= session.totalBytes then
			error("Native import is incomplete")
		end
		local started = os.clock()
		local roots = SerializationService:DeserializeInstancesAsync(session.payload)
		local expectedRoots = 0
		for _, group in ipairs(session.groups) do
			expectedRoots += group.count
		end
		if #roots ~= expectedRoots then
			error("Native import returned an unexpected root count")
		end
		local previousCamera = Workspace.CurrentCamera
		local prepared = {}
		local rootIndex = 1
		local skippedIncomingInstanceCount = 0
		for _, group in ipairs(session.groups) do
			local incoming = table.create(group.count)
			for _ = 1, group.count do
				local instance = roots[rootIndex]
				rootIndex += 1
				if instance == nil or instance.Parent ~= nil or instance:IsA("Terrain") then
					error("Native import returned an invalid root")
				end
				local protectedCamera = group.target == Workspace
					and instance:IsA("Camera")
					and (
						instance.Name == "Camera"
						or instance.Name == "CurrentCamera"
						or previousCamera ~= nil and instance.Name == previousCamera.Name
					)
				if protectedCamera then
					skippedIncomingInstanceCount += 1 + #instance:GetDescendants()
					instance:Destroy()
				else
					incoming[#incoming + 1] = instance
				end
			end
			local outgoing = {}
			for _, instance in ipairs(group.target:GetChildren()) do
				local lockedStarterContainer = group.serviceName == "StarterPlayer"
					and group.target == group.service
					and (instance:IsA("StarterPlayerScripts") or instance:IsA("StarterCharacterScripts"))
				local protectedCamera = group.target == Workspace
					and (instance == previousCamera or isProtectedWorkspaceCameraInstance(instance))
				if not instance:IsA("Terrain") and not lockedStarterContainer and not protectedCamera then
					outgoing[#outgoing + 1] = instance
				end
			end
			prepared[#prepared + 1] = {
				serviceName = group.serviceName,
				service = group.service,
				target = group.target,
				incoming = incoming,
				outgoing = outgoing,
			}
		end
		local explorerSelection = captureExplorerSelection()
		local selectionPaths = {}
		for _, instance in ipairs(explorerSelection) do
			local pathSegments = getInstancePathSegments(instance)
			if pathSegments ~= nil then
				selectionPaths[instance] = {
					pathSegments = pathSegments,
					pathOrdinals = getInstancePathOrdinals(instance),
				}
			end
		end
		local historyRecording = beginHistoryRecording("Native filesystem sync")
		local parented = {}
		local removed = {}
		local removedRootCount = 0
		local okParent, parentErr = pcall(function()
			for _, group in ipairs(prepared) do
				for _, instance in ipairs(group.incoming) do
					instance.Parent = group.target
					parented[#parented + 1] = instance
				end
				for _, instance in ipairs(group.outgoing) do
					removedRootCount += 1
					removeInstanceForUndo(instance)
					removed[#removed + 1] = { instance = instance, parent = group.target }
				end
			end
		end)
		if not okParent then
			for _, instance in ipairs(parented) do
				instance.Parent = nil
			end
			for _, entry in ipairs(removed) do
				pcall(function()
					entry.instance.Parent = entry.parent
				end)
			end
			finishHistoryRecording(historyRecording, Enum.FinishRecordingOperation.Cancel)
			restoreExplorerSelection(explorerSelection, nil)
			error(parentErr, 0)
		end
		local selectionReplacements = {}
		for instance, path in pairs(selectionPaths) do
			if instance.Parent == nil then
				local replacement = resolvePathSegments(path.pathSegments, nil, path.pathOrdinals)
				if replacement ~= nil then
					selectionReplacements[instance] = replacement
				end
			end
		end
		finishHistoryRecording(historyRecording)
		restoreExplorerSelection(explorerSelection, selectionReplacements)
		for _, group in ipairs(prepared) do
			ctx.invalidateService(group.serviceName)
		end
		local elapsed = (os.clock() - started) * 1000
		ctx.stats.requests += 1
		ctx.stats.lastMs = elapsed
		ctx.stats.lastAtUnix = os.time()
		ctx.stats.lastOk = true
		local createdInstanceCount = math.max(0, session.instanceCount - skippedIncomingInstanceCount)
		ctx.stats.instanceCreated += createdInstanceCount
		ctx.updateStatus()
		local response = {
			ok = true,
			requests = 1,
			instanceCreated = createdInstanceCount,
			rootDeleted = removedRootCount,
			binaryBytes = session.totalBytes,
			binaryMs = elapsed,
			undoRecorded = not not historyRecording,
		}
		binaryImports[importId] = nil
		completedBinaryImports[importId] = {
			response = response,
			completedAt = os.clock(),
			expiresAt = os.clock() + COMPLETED_BINARY_IMPORT_TTL_SECONDS,
		}
		pruneCompletedBinaryImports()
		return response
	end

	function api.cancelBinaryImport(params: { [string]: any }): { [string]: any }
		local importId = tostring(params.importId or "")
		local found = binaryImports[importId] ~= nil
		binaryImports[importId] = nil
		return { ok = true, found = found }
	end

	function api.cancelReconcile(params: { [string]: any }): { [string]: any }
		if type(params.service) ~= "string" or not ctx.allowedServices[params.service] then
			error("Invalid editor reconcile service")
		end
		if type(params.reconcileSession) ~= "string" or params.reconcileSession == "" then
			error("Invalid editor reconcile session id")
		end
		local serviceName = params.service
		local sessionKey = reconcileSessionKey(serviceName, params.reconcileSession)
		local session = reconcileSessions[sessionKey]
		local found = session ~= nil
		reconcileSessions[sessionKey] = nil
		if type(session) == "table" and session.rollbackSnapshot ~= nil then
			rollbackReconcileSnapshot(session.rollbackSnapshot, serviceName)
		end
		return { ok = true, found = found }
	end

	function api.applyChanges(params: { [string]: any }): { [string]: any }
		local serviceNames = validateMutationRequest(params, ctx)
		local chunkChange = nil
		for _, change in ipairs(params.instanceChanges or {}) do
			local mode = change.mode
			if mode == "beginReconcileService" or mode == "reconcileServiceChunk" or mode == "finishReconcileService" then
				if
					chunkChange ~= nil
					or #(params.instanceChanges or {}) ~= 1
					or #(params.sourceChanges or {}) > 0
					or #(params.propertyChanges or {}) > 0
				then
					error("A chunked reconcile request must contain exactly one instance change")
				end
				chunkChange = change
			end
		end
		local chunkSessionKey = nil
		local transactionSnapshot = nil
		if chunkChange ~= nil then
			chunkSessionKey = reconcileSessionKey(chunkChange.service, chunkChange.reconcileSession)
			if chunkChange.mode == "beginReconcileService" then
				if reconcileSessions[chunkSessionKey] ~= nil then
					error("Editor reconcile session already exists")
				end
				for _, activeSession in pairs(reconcileSessions) do
					if type(activeSession) == "table" and activeSession.serviceName == chunkChange.service then
						error("Another editor reconcile is already active for this service")
					end
				end
				transactionSnapshot = captureMutationSnapshot(serviceNames, params, ctx)
			else
				local session = reconcileSessions[chunkSessionKey]
				if type(session) ~= "table" or session.rollbackSnapshot == nil then
					error("Editor reconcile session was not found or cannot be rolled back")
				end
				transactionSnapshot = session.rollbackSnapshot
			end
		elseif #serviceNames > 0 then
			transactionSnapshot = captureMutationSnapshot(serviceNames, params, ctx)
		end
		local started = os.clock()
		local previousResolveCache = ctx.resolveCache
		local previousSettingsIdLookupByService = ctx.settingsIdLookupByService
		local previousMatchCandidateBuckets = ctx.matchCandidateBuckets
		local previousSelectionReplacements = ctx.selectionReplacements
		local explorerSelection = captureExplorerSelection()
		local selectionReplacements = {}
		ctx.resolveCache = {}
		ctx.settingsIdLookupByService = {}
		ctx.matchCandidateBuckets = {}
		ctx.selectionReplacements = selectionReplacements
		local historyRecording = beginHistoryRecording("Sync from filesystem")
		local stats = {
			ok = true,
			requests = 1,
			sourceCreated = 0,
			sourceUpdated = 0,
			sourceDeleted = 0,
			sourceUpdateAsync = 0,
			sourceDirect = 0,
			instanceCreated = 0,
			instanceReplaced = 0,
			instanceDeleted = 0,
			propertyUpdated = 0,
			attributeUpdated = 0,
			noops = 0,
			errors = 0,
			protectedSkipped = 0,
			protectedWrites = {},
			probeItemChanged = 0,
			probeDescendantAdded = 0,
			probeDescendantRemoving = 0,
			probeItemChangedAvailable = 0,
			probeDescendantAddedAvailable = 0,
			probeDescendantRemovingAvailable = 0,
			undoRecorded = not not historyRecording,
		}
		local touchedServices = {}
		local stopEventProbe
		if params.probeEvents == true then
			stopEventProbe = startEventProbe(stats)
		end

		local instanceChanges = params.instanceChanges
		local aborted = false
		if type(instanceChanges) == "table" then
			for _, change in ipairs(instanceChanges) do
				local ok, err = pcall(applyInstanceChange, change, ctx, stats, touchedServices)
				if not ok then
					if type(change) == "table" then
						local mode = tostring(change.mode or "")
						if mode == "beginReconcileService" or mode == "reconcileServiceChunk" or mode == "finishReconcileService" then
							local sessionKey = reconcileSessionKey(tostring(change.service or ""), change.reconcileSession)
							if mode == "finishReconcileService" then
								reconcileSessions[sessionKey] = nil
							elseif type(reconcileSessions[sessionKey]) == "table" then
								reconcileSessions[sessionKey].failed = true
							end
						end
					end
					stats.ok = false
					stats.errors += 1
					warn("[Renium] editor instance sync failed: " .. tostring(err))
					aborted = true
					break
				end
			end
		end

		local sourceChanges = params.sourceChanges
		if not aborted and type(sourceChanges) == "table" then
			for _, change in ipairs(sourceChanges) do
				local ok, err = pcall(applySourceChange, change, ctx, stats, touchedServices)
				if not ok then
					stats.ok = false
					stats.errors += 1
					warn("[Renium] editor source sync failed: " .. tostring(err))
					aborted = true
					break
				end
			end
		end
		if not aborted then
			retargetReplacementReferences(selectionReplacements, ctx, stats)
		end

		local propertyChanges = params.propertyChanges
		if not aborted and type(propertyChanges) == "table" then
			for _, change in ipairs(propertyChanges) do
				local ok, err = pcall(applyPropertyChange, change, ctx, stats, touchedServices)
				if not ok then
					stats.ok = false
					stats.errors += 1
					warn("[Renium] editor property sync failed: " .. tostring(err))
					aborted = true
					break
				end
			end
		end

		if not aborted and chunkChange ~= nil and chunkChange.mode == "beginReconcileService" then
			local session = reconcileSessions[chunkSessionKey]
			if type(session) ~= "table" then
				stats.ok = false
				stats.errors += 1
				aborted = true
			else
				session.rollbackSnapshot = transactionSnapshot
				session.onExpire = function()
					rollbackReconcileSnapshot(transactionSnapshot, chunkChange.service)
				end
			end
		end

		if stopEventProbe ~= nil then
			task.wait()
			stopEventProbe()
		end
		local restoredSelectionReplacements = selectionReplacements
		if aborted and transactionSnapshot ~= nil then
			if chunkSessionKey ~= nil then
				reconcileSessions[chunkSessionKey] = nil
			end
			local okRollback, replacements = pcall(
				restoreMutationSnapshot,
				transactionSnapshot,
				ctx,
				selectionReplacements
			)
			if okRollback then
				restoredSelectionReplacements = replacements
				stats.sourceCreated = 0
				stats.sourceUpdated = 0
				stats.sourceDeleted = 0
				stats.sourceUpdateAsync = 0
				stats.sourceDirect = 0
				stats.instanceCreated = 0
				stats.instanceReplaced = 0
				stats.instanceDeleted = 0
				stats.propertyUpdated = 0
				stats.attributeUpdated = 0
			else
				stats.errors += 1
				warn("[Renium] editor rollback failed: " .. tostring(replacements))
			end
		end
		finishHistoryRecording(
			historyRecording,
			if aborted then Enum.FinishRecordingOperation.Cancel else Enum.FinishRecordingOperation.Commit
		)
		restoreExplorerSelection(explorerSelection, restoredSelectionReplacements)
		stats.lastMs = (os.clock() - started) * 1000
		for serviceName in pairs(touchedServices) do
			ctx.invalidateService(serviceName)
		end
		ctx.resolveCache = previousResolveCache
		ctx.settingsIdLookupByService = previousSettingsIdLookupByService
		ctx.matchCandidateBuckets = previousMatchCandidateBuckets
		ctx.selectionReplacements = previousSelectionReplacements
		ctx.stats.requests += 1
		ctx.stats.lastMs = stats.lastMs
		ctx.stats.lastAtUnix = os.time()
		ctx.stats.lastOk = stats.ok
		ctx.stats.sourceCreated += stats.sourceCreated
		ctx.stats.sourceUpdated += stats.sourceUpdated
		ctx.stats.sourceDeleted += stats.sourceDeleted
		ctx.stats.sourceUpdateAsync += stats.sourceUpdateAsync
		ctx.stats.sourceDirect += stats.sourceDirect
		ctx.stats.instanceCreated += stats.instanceCreated
		ctx.stats.instanceReplaced += stats.instanceReplaced
		ctx.stats.instanceDeleted += stats.instanceDeleted
		ctx.stats.propertyUpdated += stats.propertyUpdated
		ctx.stats.attributeUpdated += stats.attributeUpdated
		ctx.stats.noops += stats.noops
		ctx.stats.errors += stats.errors
		ctx.updateStatus()
		return stats
	end

	function api.cleanup()
		local activeReconciles = {}
		for _, session in pairs(reconcileSessions) do
			if type(session) == "table" and session.rollbackSnapshot ~= nil then
				table.insert(activeReconciles, session)
			end
		end
		table.clear(reconcileSessions)
		for _, session in ipairs(activeReconciles) do
			local okRollback, rollbackError = pcall(
				rollbackReconcileSnapshot,
				session.rollbackSnapshot,
				session.serviceName
			)
			if not okRollback then
				warn("[Renium] reconcile cleanup failed: " .. tostring(rollbackError))
			end
		end
		table.clear(binaryImports)
		table.clear(completedBinaryImports)
		table.clear(binaryExports)
		if type(ctx.matchedSettingsInstancesByService) == "table" then
			table.clear(ctx.matchedSettingsInstancesByService)
		end
	end

	return api
end

return BridgeEditorSync
