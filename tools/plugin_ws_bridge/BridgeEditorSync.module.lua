local BridgeEditorSync = {}

local BridgeCandidateMatch = require(script.Parent.BridgeCandidateMatch)
local BridgeIdentity = require(script.Parent.BridgeIdentity)
local BridgeInstanceSwap = require(script.Parent.BridgeInstanceSwap)
local BridgeMaterialService = require(script.Parent.BridgeMaterialService)
local BridgeReferenceOverlay = require(script.Parent.BridgeReferenceOverlay)
local BridgeReferenceRetarget = require(script.Parent.BridgeReferenceRetarget)
local BridgeScriptDocuments = require(script.Parent.BridgeScriptDocuments)
local BridgeValueEquality = require(script.Parent.BridgeValueEquality)
local BridgeValueCodec = require(script.Parent.BridgeValueCodec)
local AssetService = game:GetService("AssetService")
local ChangeHistoryService = game:GetService("ChangeHistoryService")
local CollectionService = game:GetService("CollectionService")
local ContentProvider = game:GetService("ContentProvider")
local EncodingService = game:GetService("EncodingService")
local RunService = game:GetService("RunService")
local Selection = game:GetService("Selection")
local SerializationService = game:GetService("SerializationService")
local Workspace = game:GetService("Workspace")

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

local PATH_SEPARATOR = BridgeIdentity.PATH_SEPARATOR
local reconcileSessions = {}
local binaryImports = {}
local completedBinaryImports = {}
local binaryExports = {}
local editorTransactions = {}
local SESSION_TTL_SECONDS = 120
local MAX_RECONCILE_SESSIONS = 16
local MAX_RECONCILE_ENTRIES = 1000000
local MAX_BINARY_IMPORT_SESSIONS = 4
local MAX_BINARY_IMPORT_BUFFERED_BYTES = 536870912
local BINARY_IMPORT_CHUNK_BYTES = 2097152
local COMPLETED_BINARY_IMPORT_TTL_SECONDS = 300
local MAX_COMPLETED_BINARY_IMPORTS = 64
local NATIVE_SERIALIZATION_SERVICE_LIMIT = 4096
local NATIVE_SERIALIZATION_BATCH_LIMIT = 8192
local MESH_PART_APPLY_YIELD_INTERVAL = 4

local function binaryReadRange(params: { [string]: any }, totalBytes: number, label: string): (number, number)
	local offset = tonumber(params.offset)
	local length = tonumber(params.length)
	if not offset or offset < 0 or offset % 1 ~= 0 then
		error(`Invalid {label} offset`)
	end
	if not length or length < 1 or length > 8388608 or length % 1 ~= 0 then
		error(`Invalid {label} length`)
	end
	if params.clampLength == true then
		length = math.min(length, totalBytes - offset)
	end
	if length < 1 or offset + length > totalBytes then
		error(`{label} range exceeds its payload`)
	end
	return offset, length
end

local function encodeBinaryChunk(
	chunk: buffer,
	offset: number,
	length: number,
	totalBytes: number,
	serializationComplete: boolean
): { [string]: any }
	local encodeStarted = os.clock()
	local encoded = buffer.tostring(EncodingService:Base64Encode(chunk))
	return {
		start = offset + 1,
		nextStart = offset + length + 1,
		total = totalBytes,
		chunk = encoded,
		pluginEncodeMs = (os.clock() - encodeStarted) * 1000,
		serializationComplete = serializationComplete,
	}
end

local function countEntries(values: { [any]: any }): number
	local count = 0
	for _ in pairs(values) do
		count += 1
	end
	return count
end

local function expireSession(values: { [any]: any }, key: any, session: { [any]: any }): boolean
	if values[key] ~= session then
		return false
	end
	if (session.activeOperations or 0) > 0 then
		session.expireRequested = true
		return false
	end
	values[key] = nil
	session.expireRequested = nil
	local onExpire = session.onExpire
	session.onExpire = nil
	if type(onExpire) == "function" then
		local okExpire, expireError = pcall(onExpire)
		if not okExpire then
			warn("[Renium] session expiry cleanup failed: " .. tostring(expireError))
		end
	end
	return true
end

local function beginSessionOperation(session: { [any]: any })
	session.activeOperations = (session.activeOperations or 0) + 1
	session.updatedAt = os.clock()
end

local function endSessionOperation(values: { [any]: any }, key: any, session: { [any]: any })
	session.activeOperations -= 1
	if session.activeOperations == 0 and session.expireRequested then
		expireSession(values, key, session)
	end
end

local function runWithStudioChangeSuppression(ctx: { [string]: any }, operation)
	ctx.beginStudioChangeSuppression(nil)
	local result = table.pack(xpcall(operation, debug.traceback))
	ctx.endStudioChangeSuppression()
	if not result[1] then
		error(result[2], 0)
	end
	return table.unpack(result, 2, result.n)
end

local function runSessionOperation(values: { [any]: any }, key: any, session: { [any]: any }, operation)
	beginSessionOperation(session)
	local result = table.pack(xpcall(operation, debug.traceback))
	endSessionOperation(values, key, session)
	if not result[1] then
		error(result[2], 0)
	end
	return table.unpack(result, 2, result.n)
end

local function pruneExpiredSessions(values: { [any]: any })
	local now = os.clock()
	for key, session in pairs(values) do
		if now - session.updatedAt > SESSION_TTL_SECONDS then
			expireSession(values, key, session)
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
	if values[key] ~= session then
		return
	end
	session.updatedAt = os.clock()
	if session.expiryArmed then
		return
	end
	session.expiryArmed = true
	local function expireWhenIdle()
		if values[key] ~= session then
			return
		end
		local remaining = SESSION_TTL_SECONDS - (os.clock() - session.updatedAt)
		if remaining > 0 then
			task.delay(remaining, expireWhenIdle)
			return
		end
		session.expiryArmed = nil
		expireSession(values, key, session)
	end
	task.delay(SESSION_TTL_SECONDS, expireWhenIdle)
end

local function beginHistoryRecording(label: string): any?
	return ChangeHistoryService:TryBeginRecording(`Renium:{label}:{os.clock()}`, "Renium: " .. label)
end

local function finishHistoryRecording(recording: any?, operation: any?)
	if recording == nil then
		return
	end
	local finishOperation = operation or Enum.FinishRecordingOperation.Commit
	ChangeHistoryService:FinishRecording(recording, finishOperation)
end

local function captureExplorerSelection(): { Instance }
	return Selection:Get()
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

local function cancelExpectedEvent(ctx: { [string]: any }?, token: any)
	if token ~= nil and ctx ~= nil then
		ctx.cancelExpectedEvent(token)
	end
end

local function setParentForSync(instance: Instance, parent: Instance?, ctx: { [string]: any }?)
	if instance.Parent == parent then
		return
	end
	local token = if ctx ~= nil then ctx.expectParentChange(instance, parent) else nil
	local ok, result = pcall(function()
		instance.Parent = parent
	end)
	if not ok then
		cancelExpectedEvent(ctx, token)
		error(result, 0)
	end
end

local function setNameForSync(instance: Instance, name: string, ctx: { [string]: any }?)
	if instance.Name == name then
		return
	end
	local token = if instance:IsDescendantOf(game) and ctx ~= nil
		then ctx.expectPropertyEvent(instance, "Name", name)
		else nil
	local ok, result = pcall(function()
		instance.Name = name
	end)
	if not ok then
		cancelExpectedEvent(ctx, token)
		error(result, 0)
	end
end

local function setCurrentCameraForSync(camera: Camera?, ctx: { [string]: any }?)
	if Workspace.CurrentCamera == camera then
		return
	end
	local token = if ctx ~= nil then ctx.expectPropertyEvent(Workspace, "CurrentCamera", camera) else nil
	local ok, result = pcall(function()
		Workspace.CurrentCamera = camera
	end)
	if not ok then
		cancelExpectedEvent(ctx, token)
		error(result, 0)
	end
end

local function removeInstanceForUndo(instance: Instance, ctx: { [string]: any }?)
	setParentForSync(instance, nil, ctx)
end

local pathKey = BridgeIdentity.pathKey
local pathCacheKey = BridgeIdentity.pathCacheKey
local resolveOrdinalChild = BridgeIdentity.resolveOrdinalChild
local resolvePathSegments = BridgeIdentity.resolvePathSegments

local function containsPackageLink(root: Instance): boolean
	return root:IsA("PackageLink") or root:FindFirstChildWhichIsA("PackageLink", true) ~= nil
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

local liveInstance = BridgeIdentity.liveInstance

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
	if serviceName == "" then
		return nil
	end
	return ctx.getState(serviceName)
end

local function parseInstanceIndexId(settingsId: string, identityModule: any): number?
	local index = identityModule.parseInstanceIndexId(settingsId)
	if type(index) == "number" then
		return index
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
				local debugId = identityModule.getCachedDebugId(state, instance)
				if type(debugId) == "string" and debugId ~= "" then
					lookup["debug:" .. debugId] = instance
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
	if index and index >= 1 then
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

local function resolveInstance(
	change: { [string]: any },
	ctx: { [string]: any },
	allowClassMismatch: boolean?
): Instance?
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
	if type(pathSegments) == "table" and ctx.resolveStagedPath ~= nil then
		local staged = ctx.resolveStagedPath(pathSegments, change.pathOrdinals)
		if staged ~= nil and (allowClassMismatch or instanceMatchesExpectedClass(staged, change.className)) then
			return staged
		end
	end
	local persistent = matchedSettingsInstance(serviceName, change.settingsId, ctx)
	if persistent ~= nil and (allowClassMismatch or instanceMatchesExpectedClass(persistent, change.className)) then
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
		if
			pathInstance ~= nil and (allowClassMismatch or instanceMatchesExpectedClass(pathInstance, change.className))
		then
			return pathInstance
		end
	end
	if instance ~= nil and (allowClassMismatch or instanceMatchesExpectedClass(instance, change.className)) then
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

local function syncEntryPlacement(
	entry: { [string]: any },
	instance: Instance,
	stats: { [string]: any },
	resolvedEntries: { [string]: any }?,
	ctx: { [string]: any }
)
	local parent = resolveEntryParent(entry, resolvedEntries)
	if parent == nil then
		error("Cannot place instance; parent path was not found: " .. tostring(entry.key))
	end
	if instance.Parent ~= parent then
		setParentForSync(instance, parent, ctx)
		stats.propertyUpdated += 1
	end
	local nextName = tostring(entry.pathSegments[#entry.pathSegments] or instance.Name)
	if nextName ~= "" and instance.Name ~= nextName then
		setNameForSync(instance, nextName, ctx)
		stats.propertyUpdated += 1
	end
end

local function instanceSettingsIdKeys(
	serviceName: string,
	instance: Instance,
	ctx: { [string]: any }
): { [string]: boolean }
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
	local index = identityModule.getCachedInstanceIndex(state, instance)
	if type(index) == "number" and index >= 1 then
		keys[string.format("%x", index)] = true
	end
	local debugId = identityModule.getCachedDebugId(state, instance)
	if type(debugId) == "string" and debugId ~= "" then
		keys["debug:" .. debugId] = true
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
		local indexedInstance = if index then liveInstance(state.instances[index]) else nil
		if not index or indexedInstance ~= oldInstance then
			for candidateIndex, candidate in ipairs(state.instances) do
				if candidate == oldInstance then
					index = candidateIndex
					break
				end
			end
		end
		if index then
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
		elseif index then
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
	if not next(desiredSettingsIds) then
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
	if key ~= "" and desiredStableKeys[key] then
		return false
	end
	return key ~= "" and desiredKeys[key] or false
end

local function assertAllowedService(serviceName: string, ctx: { [string]: any }): Instance
	if serviceName == "" or not ctx.allowedServices[serviceName] then
		error("Refusing editor mutation outside an allowed service: " .. tostring(serviceName))
	end
	return game:GetService(serviceName)
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

local function decodeRefValue(raw: { [string]: any }, ctx: { [string]: any }?, serviceName: string?): any
	local targetServiceName = if type(raw.pathSegments) == "table" and #raw.pathSegments > 0
		then tostring(raw.pathSegments[1])
		else serviceName
	if ctx ~= nil and ctx.resolveStagedPath ~= nil and type(raw.pathSegments) == "table" then
		local staged = ctx.resolveStagedPath(raw.pathSegments, raw.pathOrdinals)
		if staged ~= nil then
			return staged
		end
	end
	if type(raw.pathSegments) == "table" then
		local instance = resolvePathSegments(raw.pathSegments, nil, raw.pathOrdinals)
		if instance ~= nil then
			return instance
		end
	end
	local settingsInstance: Instance? = nil
	if ctx ~= nil and type(targetServiceName) == "string" and targetServiceName ~= "" then
		local settingsId = raw.settingsId
			or raw.instanceId
			or (if type(raw.debugId) == "string" and raw.debugId ~= "" then "debug:" .. raw.debugId else nil)
		local persistent = matchedSettingsInstance(targetServiceName, settingsId, ctx)
		if persistent ~= nil then
			return persistent
		end
		settingsInstance = resolveInstanceBySettingsId(targetServiceName, settingsId, ctx)
		if settingsInstance ~= nil and strongSettingsId(settingsId) then
			return settingsInstance
		end
	end
	return settingsInstance
end

local function decodeValue(raw: any, enumHint: string?, ctx: { [string]: any }?, serviceName: string?): (boolean, any)
	return BridgeValueCodec.decode(raw, enumHint, decodeRefValue, ctx, serviceName)
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
		if
			propertyName == "Scale"
			or propertyName == "WorldPivot"
			or propertyName == "WorldPivotData"
			or propertyName == "Origin"
		then
			return true
		end
	end
	return RbxDomModule.findCanonicalPropertyDescriptor(instance.ClassName, propertyName) ~= nil
end

local function decodePropertyValue(
	instance: Instance,
	propertyName: string,
	rawValue: any,
	ctx: { [string]: any },
	serviceName: string
): (boolean, any)
	if type(rawValue) == "table" and rawValue._type == nil then
		local okCurrent, current = pcall(function()
			return (instance :: any)[propertyName]
		end)
		if okCurrent and typeof(current) == "NumberRange" then
			rawValue = table.clone(rawValue)
			rawValue._type = "NumberRange"
		end
	end
	return decodeValue(rawValue, enumHintForProperty(instance, propertyName), ctx, serviceName)
end

local valuesEqual = BridgeValueEquality.valuesEqual

BridgeEditorSync.decodeValue = decodeValue
BridgeEditorSync.valuesEqual = valuesEqual

local function connectProbeSignal(
	stats: { [string]: any },
	eventName: string,
	countField: string,
	availableField: string,
	connections: { RBXScriptConnection }
)
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
	connectProbeSignal(
		stats,
		"DescendantRemoving",
		"probeDescendantRemoving",
		"probeDescendantRemovingAvailable",
		connections
	)

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
	local isMaterialOverride, materialOverride = BridgeMaterialService.readOverride(instance, propertyName)
	if isMaterialOverride then
		return true, materialOverride
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
	if settingsInstance ~= nil and not claimedInstances[settingsInstance] and strongSettingsId(entry.settingsId) then
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
			return pcall((instance :: any).ScaleTo, instance, value)
		elseif propertyName == "Origin" then
			return pcall((instance :: any).PivotTo, instance, value)
		elseif propertyName == "WorldPivot" or propertyName == "WorldPivotData" then
			return pcall(function()
				(instance :: any).WorldPivot = value
			end)
		end
	end
	local isMaterialOverride, materialOverrideError = BridgeMaterialService.writeOverride(instance, propertyName, value)
	if isMaterialOverride then
		return true, nil
	elseif materialOverrideError ~= nil then
		return false, materialOverrideError
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

local function writePropertyForSync(
	instance: Instance,
	propertyName: string,
	value: any,
	ctx: { [string]: any }?
): (boolean, any)
	local token = if instance:IsDescendantOf(game) and ctx ~= nil
		then ctx.expectPropertyEvent(instance, propertyName, value)
		else nil
	local ok, result = writeProperty(instance, propertyName, value)
	if not ok then
		cancelExpectedEvent(ctx, token)
	end
	return ok, result
end

local function setAttributeForSync(
	instance: Instance,
	attributeName: string,
	value: any,
	ctx: { [string]: any }?
): (boolean, any)
	local token = if instance:IsDescendantOf(game) and ctx ~= nil
		then ctx.expectAttributeEvent(instance, attributeName, value)
		else nil
	local ok, result = pcall(instance.SetAttribute, instance, attributeName, value)
	if not ok then
		cancelExpectedEvent(ctx, token)
	end
	return ok, result
end

local function meshPartSourceKey(meshId: string, meshPart: MeshPart): string
	return meshId
		.. PATH_SEPARATOR
		.. tostring(meshPart.CollisionFidelity.Value)
		.. PATH_SEPARATOR
		.. tostring(meshPart.RenderFidelity.Value)
		.. PATH_SEPARATOR
		.. tostring(meshPart.FluidFidelity.Value)
end

local function loadedMeshPartSources(ctx: { [string]: any }): { [string]: MeshPart }
	if ctx.loadedMeshPartSources == nil then
		local sources = {}
		for _, candidate in game:GetDescendants() do
			if candidate:IsA("MeshPart") and candidate.MeshId ~= "" then
				sources[meshPartSourceKey(candidate.MeshId, candidate)] = candidate
			end
		end
		ctx.loadedMeshPartSources = sources
	end
	return ctx.loadedMeshPartSources
end

local function findLoadedMeshPartSource(target: MeshPart, meshId: string, ctx: { [string]: any }): MeshPart?
	local sources = loadedMeshPartSources(ctx)
	local key = meshPartSourceKey(meshId, target)
	local source = sources[key]
	if
		source == nil
		or source == target
		or source.Parent == nil
		or source.MeshId ~= meshId
		or source.CollisionFidelity ~= target.CollisionFidelity
		or source.RenderFidelity ~= target.RenderFidelity
		or source.FluidFidelity ~= target.FluidFidelity
	then
		sources[key] = nil
		return nil
	end
	return source
end

local function rememberLoadedMeshPartSource(meshPart: MeshPart, ctx: { [string]: any })
	if ctx.loadedMeshPartSources ~= nil and meshPart.MeshId ~= "" then
		ctx.loadedMeshPartSources[meshPartSourceKey(meshPart.MeshId, meshPart)] = meshPart
	end
end

local function preloadPropertyMeshPartSources(changes: { any }, ctx: { [string]: any }): (number, number)
	local readySources = {}
	local sources = {}

	for _, change in ipairs(changes) do
		local properties = if type(change) == "table" then change.properties else nil
		local rawMeshId = if type(properties) == "table" then properties.MeshId else nil
		if rawMeshId == nil then
			continue
		end

		local instance = resolveInstance(change, ctx)
		if instance == nil or not instance:IsA("MeshPart") then
			continue
		end

		local serviceName = tostring(change.service or "")
		local okDecode, decoded = decodePropertyValue(instance, "MeshId", rawMeshId, ctx, serviceName)
		local meshId = if okDecode then tostring(decoded or "") else ""
		local source = if meshId ~= "" then findLoadedMeshPartSource(instance, meshId, ctx) else nil
		if source ~= nil and not readySources[source] then
			readySources[source] = true
			sources[#sources + 1] = source
		end
	end

	ctx.readyMeshPartSources = readySources
	if #sources == 0 then
		return 0, 0
	end

	local started = os.clock()
	ContentProvider:PreloadAsync(sources)
	return #sources, (os.clock() - started) * 1000
end

local function applyMeshPartMeshId(instance: Instance, meshId: any, ctx: { [string]: any }): (boolean, any)
	if not instance:IsA("MeshPart") then
		return false, "MeshId can only be applied to MeshPart"
	end

	local targetMeshPart = instance :: MeshPart
	local meshIdText = tostring(meshId or "")
	local sourceMeshPart
	local destroySource = false
	local sourceReady = false
	if meshIdText == "" then
		sourceMeshPart = Instance.new("MeshPart")
		destroySource = true
		sourceReady = true
	else
		sourceMeshPart = findLoadedMeshPartSource(targetMeshPart, meshIdText, ctx)
		if sourceMeshPart == nil then
			local okContent, meshContent = pcall((Content :: any).fromUri, meshIdText)
			if not okContent then
				return false, meshContent
			end
			local okCreate, meshPartOrErr = pcall(AssetService.CreateMeshPartAsync, AssetService, meshContent, {
				CollisionFidelity = targetMeshPart.CollisionFidelity,
				RenderFidelity = targetMeshPart.RenderFidelity,
				FluidFidelity = targetMeshPart.FluidFidelity,
			})
			if not okCreate or meshPartOrErr == nil then
				return false, meshPartOrErr
			end
			sourceMeshPart = meshPartOrErr
			destroySource = true
			sourceReady = true
		elseif ctx.readyMeshPartSources ~= nil then
			sourceReady = ctx.readyMeshPartSources[sourceMeshPart]
		end
	end

	local targetTextureContent = targetMeshPart.TextureContent
	local targetSize = targetMeshPart.Size
	local applyTokens = {}
	if targetMeshPart:IsDescendantOf(game) then
		for _, propertyName in ipairs({
			"MeshId",
			"MeshContent",
			"TextureID",
			"TextureContent",
			"Size",
			"CollisionFidelity",
			"RenderFidelity",
			"FluidFidelity",
		}) do
			local okValue, value = pcall(function()
				return (sourceMeshPart :: any)[propertyName]
			end)
			if okValue then
				applyTokens[#applyTokens + 1] = ctx.expectPropertyEvent(targetMeshPart, propertyName, value)
			end
		end
	end
	local okApply, applyErr = pcall(targetMeshPart.ApplyMesh, targetMeshPart, sourceMeshPart)
	if destroySource then
		sourceMeshPart:Destroy()
	end
	if not okApply then
		for _, token in ipairs(applyTokens) do
			cancelExpectedEvent(ctx, token)
		end
		return false, applyErr
	end
	local restoreTokens = {}
	local okRestore, restoreErr = pcall(function()
		if targetMeshPart:IsDescendantOf(game) then
			restoreTokens[#restoreTokens + 1] = ctx.expectPropertyEvent(targetMeshPart, "Size", targetSize)
			restoreTokens[#restoreTokens + 1] =
				ctx.expectPropertyEvent(targetMeshPart, "TextureContent", targetTextureContent)
		end
		targetMeshPart.Size = targetSize
		targetMeshPart.TextureContent = targetTextureContent
	end)
	if not okRestore then
		for _, token in ipairs(restoreTokens) do
			cancelExpectedEvent(ctx, token)
		end
		return false, restoreErr
	end
	rememberLoadedMeshPartSource(targetMeshPart, ctx)
	if ctx.readyMeshPartSources ~= nil then
		ctx.readyMeshPartSources[targetMeshPart] = true
	end
	if not sourceReady then
		ctx.meshPartApplyCount += 1
		if ctx.meshPartApplyCount % MESH_PART_APPLY_YIELD_INTERVAL == 0 then
			RunService.Heartbeat:Wait()
		end
	end
	return true, nil
end

local function setTagForSync(instance: Instance, tag: string, added: boolean, ctx: { [string]: any })
	local token = if instance:IsDescendantOf(game) then ctx.expectTagChange(instance, tag, added) else nil
	local ok, result = pcall(function()
		if added then
			CollectionService:AddTag(instance, tag)
		else
			CollectionService:RemoveTag(instance, tag)
		end
	end)
	if not ok then
		cancelExpectedEvent(ctx, token)
		error(result, 0)
	end
end

local function applyTags(instance: Instance, rawTags: any, stats: { [string]: any }, ctx: { [string]: any })
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
			setTagForSync(instance, tag, false, ctx)
			changed = true
		end
		desired[tag] = nil
	end
	for tag in pairs(desired) do
		setTagForSync(instance, tag, true, ctx)
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
	selectionReplacements: { [Instance]: Instance }?,
	ctx: { [string]: any }
): Instance
	if instance.ClassName == className then
		stats.noops += 1
		return instance
	end

	local replacement = BridgeInstanceSwap.replace(
		instance,
		className,
		CollectionService,
		function(target)
			removeInstanceForUndo(target, ctx)
		end,
		nil,
		function(target, parent)
			setParentForSync(target, parent, ctx)
		end
	)
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
	if not next(replacements) then
		return
	end
	local roots = {}
	for serviceName, allowed in pairs(ctx.allowedServices) do
		if allowed then
			roots[#roots + 1] = game:GetService(serviceName)
		end
	end
	local updated, failed, failures = BridgeReferenceRetarget.apply(
		roots,
		replacements,
		RbxDomModule.getReferencePropertyNames,
		readProperty,
		function(instance, propertyName, value)
			return writePropertyForSync(instance, propertyName, value, ctx)
		end
	)
	stats.propertyUpdated += updated
	if failed > 0 then
		local first = failures[1]
		error(
			`Could not retarget {failed} references after class replacement; first failure: {first.instance:GetFullName()}.{first.propertyName}: {first.error}`
		)
	end
end

local readScriptSource = BridgeScriptDocuments.readSource
local setSource = BridgeScriptDocuments.setSource
local ScriptDocumentState = BridgeScriptDocuments

local ReferenceOverlay = BridgeReferenceOverlay.create({
	BridgeIdentity = BridgeIdentity,
	BridgeReferenceRetarget = BridgeReferenceRetarget,
	RbxDomModule = RbxDomModule,
	captureExplorerSelection = captureExplorerSelection,
	containsPackageLink = containsPackageLink,
	pathCacheKey = pathCacheKey,
	readProperty = readProperty,
	removeInstanceForUndo = removeInstanceForUndo,
	resolveOrdinalChild = resolveOrdinalChild,
	resolvePathSegments = resolvePathSegments,
	restoreExplorerSelection = restoreExplorerSelection,
	setCurrentCameraForSync = setCurrentCameraForSync,
	setParentForSync = setParentForSync,
	writePropertyForSync = writePropertyForSync,
})

local function syncOptions(ctx: { [string]: any }): { [string]: any }
	return ctx.getSyncOptions()
end

local function liveHydrateEnabled(ctx: { [string]: any }): boolean
	return syncOptions(ctx).liveHydrate ~= false
end

local function keepUnknownsEnabled(ctx: { [string]: any }): boolean
	return syncOptions(ctx).keepUnknowns == true
end

local function includeManagedInstance(ctx: { [string]: any }, serviceName: string, instance: Instance): boolean
	return ctx.includeExportInstance(serviceName, instance)
end

local function ensureSourceParentPath(
	change: { [string]: any },
	service: Instance,
	stats: { [string]: any },
	ctx: { [string]: any }
): Instance?
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
				setNameForSync(folder, name, ctx)
				setParentForSync(folder, current, ctx)
				stats.instanceCreated += 1
				existing += 1
				child = folder
			end
		end
		current = child
	end
	return current
end

local function applySourceChange(
	change: { [string]: any },
	ctx: { [string]: any },
	stats: { [string]: any },
	touchedServices: { [string]: boolean }
)
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
			local okWrite, err, writeMethod = setSource(instance, "", ctx)
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
		local parent = resolveParent(change, ctx.resolveCache) or ensureSourceParentPath(change, service, stats, ctx)
		if parent == nil then
			error("Cannot create source instance; parent path was not found")
		end
		assertInstanceInService(parent, service)
		local okCreate, created = pcall(Instance.new, tostring(change.className or "ModuleScript"))
		if not okCreate or created == nil then
			error("Cannot create source instance: " .. tostring(created))
		end
		local pathSegments = cloneArray(change.pathSegments)
		setNameForSync(created, tostring(pathSegments[#pathSegments] or created.ClassName), ctx)
		setParentForSync(created, parent, ctx)
		if type(ctx.resolveCache) == "table" then
			ctx.resolveCache[pathCacheKey(change.pathSegments, change.pathOrdinals)] = created
		end
		instance = created
		stats.sourceCreated += 1
	end

	if instance.ClassName == "Folder" and ctx.luaSourceClass[tostring(change.className or "")] then
		local oldInstance = instance
		instance = replaceInstanceClass(instance, tostring(change.className), stats, ctx.selectionReplacements, ctx)
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

	local okWrite, err, writeMethod = setSource(instance, nextSource, ctx)
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

local appendPreserveKeys

local function sortedInstanceEntries(
	change: { [string]: any },
	serviceName: string,
	maximumEntries: number?,
	limitError: string?
)
	local rawInstances = change.instances
	local entries = {}
	if type(rawInstances) ~= "table" then
		return entries
	end
	if #rawInstances > (maximumEntries or 5000) then
		error(limitError or "Editor instance mutation has too many entries")
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
					entries[#entries + 1] = {
						pathSegments = pathSegments,
						pathOrdinals = cloneArray(raw.pathOrdinals),
						key = pathCacheKey(pathSegments, raw.pathOrdinals),
						className = className,
						settingsId = settingsIdText(raw.settingsId),
						ambiguousSiblings = raw.ambiguousSiblings == true,
						anchorOnly = raw.anchorOnly == true,
						matchProperties = if type(raw.matchProperties) == "table" then raw.matchProperties else {},
						matchAttributes = if type(raw.matchAttributes) == "table" then raw.matchAttributes else {},
					}
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

local function syncDesiredEntry(
	entry: { [string]: any },
	serviceName: string,
	ctx: { [string]: any },
	stats: { [string]: any },
	resolvedEntries: { [string]: any },
	claimedInstances: { [Instance]: boolean },
	createMissing: boolean
): Instance?
	if isProtectedWorkspaceCameraPath(entry.pathSegments) then
		stats.noops += 1
		return nil
	end

	local instance = resolveEntryInstance(entry, serviceName, ctx, resolvedEntries, claimedInstances)
	if entry.anchorOnly and instance == nil then
		error("Filtered ancestor was not found: " .. entry.key)
	elseif entry.anchorOnly then
		stats.noops += 1
	elseif instance == nil and not createMissing then
		stats.noops += 1
		return nil
	elseif instance == nil then
		local parent = resolveEntryParent(entry, resolvedEntries)
		if parent == nil then
			error("Cannot create instance; parent path was not found: " .. entry.key)
		end
		local okCreate, created = pcall(Instance.new, entry.className)
		if not okCreate or created == nil then
			error(`Cannot create {entry.className} at {pathKey(entry.pathSegments)}: {created}`)
		end
		created.Name = tostring(entry.pathSegments[#entry.pathSegments])
		setParentForSync(created, parent, ctx)
		instance = created
		stats.instanceCreated += 1
	else
		syncEntryPlacement(entry, instance, stats, resolvedEntries, ctx)
		if instance.ClassName ~= entry.className then
			local oldInstance = instance
			instance = replaceInstanceClass(instance, entry.className, stats, ctx.selectionReplacements, ctx)
			rememberReplacementIdentity(serviceName, entry.settingsId, oldInstance, instance, ctx)
		end
	end

	resolvedEntries[entry.key] = instance
	rememberEntryResolution(entry, serviceName, instance, claimedInstances, ctx)
	return instance
end

local function removeUnknownInstances(
	serviceName: string,
	service: Instance,
	ctx: { [string]: any },
	stats: { [string]: any },
	preserveKeys: { [string]: boolean },
	desiredKeys: { [string]: boolean },
	desiredSettingsIds: { [string]: boolean },
	desiredStableKeys: { [string]: boolean }
)
	local descendants = service:GetDescendants()
	for index = #descendants, 1, -1 do
		local instance = descendants[index]
		local pathSegments, pathOrdinals = BridgeIdentity.getRefPathParts(instance)
		local key = if pathSegments then pathCacheKey(pathSegments, pathOrdinals) else ""
		if
			key ~= ""
			and not preserveKeys[key]
			and includeManagedInstance(ctx, serviceName, instance)
			and not shouldKeepInstanceByDesiredEntry(
				serviceName,
				instance,
				pathSegments,
				pathOrdinals,
				ctx,
				desiredKeys,
				desiredSettingsIds,
				desiredStableKeys
			)
			and not isProtectedWorkspaceCameraInstance(instance)
		then
			removeInstanceForUndo(instance, ctx)
			stats.instanceDeleted += 1
		end
	end
end

local function applyInstanceReconcile(
	change: { [string]: any },
	ctx: { [string]: any },
	stats: { [string]: any },
	touchedServices: { [string]: boolean }
)
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

	local beforeCreated = stats.instanceCreated
	local beforeDeleted = stats.instanceDeleted
	local beforeReplaced = stats.instanceReplaced
	local desiredKeys = {}
	local desiredSettingsIds = {}
	local desiredStableKeys = {}
	local desiredEntries = sortedInstanceEntries(
		change,
		service.Name,
		tonumber(ctx.maxInstanceEntriesPerChange) or 5000,
		"Editor instance reconcile has too many entries"
	)
	local preserveKeys = {}
	appendPreserveKeys(preserveKeys, change, service.Name)

	local resolvedEntries = {}
	local claimedInstances = {}
	for _, entry in ipairs(desiredEntries) do
		desiredKeys[entry.key] = true
		local instance = syncDesiredEntry(entry, service.Name, ctx, stats, resolvedEntries, claimedInstances, true)
		if instance ~= nil then
			recordDesiredStableEntry(entry, service.Name, instance, ctx, desiredSettingsIds, desiredStableKeys)
		end
	end

	if change.allowDeletes == true and not keepUnknownsEnabled(ctx) then
		removeUnknownInstances(
			service.Name,
			service,
			ctx,
			stats,
			preserveKeys,
			desiredKeys,
			desiredSettingsIds,
			desiredStableKeys
		)
	end

	if
		stats.instanceCreated == beforeCreated
		and stats.instanceDeleted == beforeDeleted
		and stats.instanceReplaced == beforeReplaced
	then
		stats.noops += 1
	end
end

local function reconcileSessionKey(serviceName: string, sessionId: any): string
	return serviceName .. PATH_SEPARATOR .. tostring(sessionId or "default")
end

appendPreserveKeys = function(target: { [string]: boolean }, change: { [string]: any }, serviceName: string)
	for _, raw in ipairs(change.preserveInstances or {}) do
		if type(raw) == "table" then
			local pathSegments = cloneArray(raw.pathSegments)
			if #pathSegments > 1 and tostring(pathSegments[1]) == serviceName then
				target[pathCacheKey(pathSegments, raw.pathOrdinals)] = true
			end
		end
	end
end

local function applyInstanceReconcileChunk(
	change: { [string]: any },
	ctx: { [string]: any },
	stats: { [string]: any },
	touchedServices: { [string]: boolean }
)
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
			preserveKeys = {},
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
	appendPreserveKeys(session.preserveKeys, change, service.Name)
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
		local instance =
			syncDesiredEntry(entry, service.Name, ctx, stats, session.resolvedEntries, session.claimedInstances, true)
		if instance ~= nil then
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

	if mode == "finishReconcileService" then
		if not keepUnknownsEnabled(ctx) then
			removeUnknownInstances(
				service.Name,
				service,
				ctx,
				stats,
				session.preserveKeys,
				session.desiredKeys,
				session.desiredSettingsIds,
				session.desiredStableKeys
			)
		end
		reconcileSessions[sessionKey] = nil
	end

	if
		stats.instanceCreated == beforeCreated
		and stats.instanceDeleted == beforeDeleted
		and stats.instanceReplaced == beforeReplaced
	then
		stats.noops += 1
	end
end

local function applyInstanceUpserts(
	change: { [string]: any },
	ctx: { [string]: any },
	stats: { [string]: any },
	touchedServices: { [string]: boolean }
)
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true

	local beforeCreated = stats.instanceCreated
	local beforeReplaced = stats.instanceReplaced
	local resolvedEntries = {}
	local claimedInstances = {}
	local createMissing = liveHydrateEnabled(ctx)
	for _, entry in ipairs(sortedInstanceEntries(change, service.Name)) do
		syncDesiredEntry(entry, service.Name, ctx, stats, resolvedEntries, claimedInstances, createMissing)
	end

	if stats.instanceCreated == beforeCreated and stats.instanceReplaced == beforeReplaced then
		stats.noops += 1
	end
end

local function applyInstanceDeletes(
	change: { [string]: any },
	ctx: { [string]: any },
	stats: { [string]: any },
	touchedServices: { [string]: boolean }
)
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true

	local beforeDeleted = stats.instanceDeleted
	local targets = {}
	local seenTargets = {}
	for _, entry in ipairs(sortedInstanceEntries(change, service.Name)) do
		if #entry.pathSegments <= 1 then
			error("Refusing to delete service root: " .. entry.key)
		end
		local resolved = resolveInstanceBySettingsId(service.Name, entry.settingsId, ctx)
		local instance = if resolved == nil or instanceMatchesExpectedClass(resolved, entry.className)
			then resolved
			else nil
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
		removeInstanceForUndo(instance, ctx)
		stats.instanceDeleted += 1
	end

	if stats.instanceDeleted == beforeDeleted then
		stats.noops += 1
	end
end

local function applyInstanceChange(
	change: { [string]: any },
	ctx: { [string]: any },
	stats: { [string]: any },
	touchedServices: { [string]: boolean }
)
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

local function applyPropertyChange(
	change: { [string]: any },
	ctx: { [string]: any },
	stats: { [string]: any },
	touchedServices: { [string]: boolean }
)
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
	local staged = if ctx.resolveStagedPath ~= nil
		then ctx.resolveStagedPath(change.pathSegments, change.pathOrdinals)
		else nil
	if staged ~= instance then
		assertInstanceInService(instance, service)
	end
	if isProtectedWorkspaceCameraPath(change.pathSegments) or isProtectedWorkspaceCameraInstance(instance) then
		stats.noops += 1
		return
	end

	local properties = change.properties
	if type(properties) == "table" then
		local unreadableNames = if type(ctx.unreadablePropertyNames) == "table"
			then ctx.unreadablePropertyNames[instance]
			else nil
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
					setNameForSync(instance, nextName, ctx)
					stats.propertyUpdated += 1
				end
			elseif propertyName == "Tags" then
				applyTags(instance, rawValue, stats, ctx)
			else
				if not classHasProperty(instance, propertyName) then
					stats.noops += 1
					continue
				end
				if type(unreadableNames) == "table" and unreadableNames[propertyName] then
					recordProtectedWrite(stats, change, "property", propertyName, rawValue)
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
				else
					local okWrite, err = writePropertyForSync(instance, propertyName, decoded, ctx)
					if not okWrite then
						if propertyName == "MeshId" and instance:IsA("MeshPart") then
							local okApplyMesh, applyMeshErr = applyMeshPartMeshId(instance, decoded, ctx)
							if not okApplyMesh then
								error(`Failed to apply MeshId on {instance:GetFullName()}: {applyMeshErr}`)
							end
						else
							local errText = string.lower(tostring(err))
							if
								string.find(errText, "read only", 1, true)
								or string.find(errText, "lacking capability robloxscript", 1, true)
								or string.find(errText, "not a valid member", 1, true)
							then
								recordProtectedWrite(stats, change, "property", propertyName, rawValue)
								stats.noops += 1
								continue
							end
							error(`Failed to write {propertyName} on {instance:GetFullName()}: {err}`)
						end
					end
					stats.propertyUpdated += 1
					if
						instance:IsA("MeshPart")
						and (
							propertyName == "MeshId"
							or propertyName == "CollisionFidelity"
							or propertyName == "RenderFidelity"
							or propertyName == "FluidFidelity"
						)
					then
						rememberLoadedMeshPartSource(instance, ctx)
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
				local okWrite, err = setAttributeForSync(instance, attributeName, nil, ctx)
				if not okWrite then
					local errText = string.lower(tostring(err))
					if
						string.find(errText, "corescript permission required", 1, true)
						or string.find(errText, "read only", 1, true)
					then
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
				local okWrite, err = setAttributeForSync(instance, attributeName, decoded, ctx)
				if not okWrite then
					local errText = string.lower(tostring(err))
					if
						string.find(errText, "corescript permission required", 1, true)
						or string.find(errText, "read only", 1, true)
					then
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

local function validatedPackageDescriptors(
	rawDescriptors: any,
	label: string,
	serviceName: string,
	targetPath: { string },
	count: number
): { any }
	if rawDescriptors == nil then
		return {}
	end
	local descriptorsAreArray, descriptorCount = denseArrayLength(rawDescriptors)
	if not descriptorsAreArray or descriptorCount > count then
		error("Invalid " .. label)
	end
	local descriptors = {}
	local keys = {}
	for descriptorIndex, descriptor in ipairs(rawDescriptors) do
		validateObjectTable(descriptor, label)
		validateMutationPath(descriptor, serviceName, `{label} {descriptorIndex}`)
		local pathLength = #descriptor.pathSegments
		local className = descriptor.className
		if
			#descriptor.pathOrdinals ~= pathLength
			or pathLength ~= #targetPath + 1
			or type(className) ~= "string"
			or className == ""
		then
			error("Invalid " .. label)
		end
		for index = 1, #targetPath do
			if descriptor.pathSegments[index] ~= targetPath[index] then
				error(label .. " is outside its target")
			end
		end
		local key = pathCacheKey(descriptor.pathSegments, descriptor.pathOrdinals)
		if keys[key] then
			error("Duplicate " .. label)
		end
		keys[key] = true
		descriptors[#descriptors + 1] = {
			pathSegments = table.clone(descriptor.pathSegments),
			pathOrdinals = table.clone(descriptor.pathOrdinals),
			className = className,
		}
	end
	return descriptors
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

local function isEngineManagedContainerEntry(serviceName: string, entry: { [string]: any }): boolean
	if serviceName ~= "StarterPlayer" or type(entry.pathSegments) ~= "table" or #entry.pathSegments ~= 2 then
		return false
	end
	local className = entry.className
	return (className == "StarterPlayerScripts" or className == "StarterCharacterScripts")
		and entry.pathSegments[2] == className
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
					(
						mode == "beginReconcileService"
						or mode == "reconcileServiceChunk"
						or mode == "finishReconcileService"
					) and (type(change.reconcileSession) ~= "string" or change.reconcileSession == "")
				then
					error("Editor chunked reconcile requires a session id")
				end
				local instancesAreArray, instanceCount = denseArrayLength(change.instances)
				if not instancesAreArray or instanceCount > (tonumber(ctx.maxInstanceEntriesPerChange) or 5000) then
					error("Editor instance entries must be a bounded array")
				end
				local preservesAreArray, preserveCount = denseArrayLength(change.preserveInstances or {})
				if not preservesAreArray or preserveCount > (tonumber(ctx.maxInstanceEntriesPerChange) or 5000) then
					error("Editor preserve entries must be a bounded array")
				end
				for preserveIndex, preserve in ipairs(change.preserveInstances or {}) do
					if type(preserve) ~= "table" then
						error(string.format("Editor preserve entry %d must be an object", preserveIndex))
					end
					validateMutationPath(
						preserve,
						serviceName,
						string.format("Editor preserve entry %d", preserveIndex)
					)
				end
				for entryIndex, entry in ipairs(change.instances) do
					if type(entry) ~= "table" then
						error(string.format("Editor instance entry %d must be an object", entryIndex))
					end
					validateMutationPath({
						pathSegments = entry.pathSegments,
						pathOrdinals = entry.pathOrdinals,
					}, serviceName, string.format("Editor instance entry %d", entryIndex))
					if not isEngineManagedContainerEntry(serviceName, entry) then
						validateCreatableClass(entry.className, classCache, "Editor instance entry")
					end
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
				if
					type(change.source) == "string"
					and #change.source > (tonumber(ctx.maxSourceBytes) or 8 * 1024 * 1024)
				then
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

local function mutationSnapshotLayout(serviceNames: { string }, ctx: { [string]: any })
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
					local children = {}
					for _, child in ipairs(container:GetChildren()) do
						if includeManagedInstance(ctx, serviceName, child) then
							children[#children + 1] = child
						end
					end
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
			local included = includeManagedInstance(ctx, serviceName, child)
			if not preserved[child] and included then
				table.insert(children, child)
			elseif not included then
				preserved[child] = true
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

local captureNativeExportFingerprint
local nativeExportFingerprintMatches

local function captureMutationSnapshot(serviceNames: { string }, params: { [string]: any }, ctx: { [string]: any })
	local sourceChanges = params.sourceChanges or {}
	local hasStructuralChanges = false
	if params.skipStructural ~= true then
		hasStructuralChanges = params.forceStructural == true or #(params.instanceChanges or {}) > 0
		if not hasStructuralChanges then
			for _, change in ipairs(sourceChanges) do
				local instance = resolveInstance(change, ctx, true)
				if
					(instance == nil and change.deleted ~= true and liveHydrateEnabled(ctx))
					or (
						instance ~= nil
						and instance.ClassName == "Folder"
						and ctx.luaSourceClass[tostring(change.className or "")]
					)
				then
					hasStructuralChanges = true
					break
				end
			end
		end
	end
	local groups, roots, metadataTargets, metadataSeen
	if hasStructuralChanges then
		groups, roots, metadataTargets, metadataSeen = mutationSnapshotLayout(serviceNames, ctx)
	else
		groups, roots, metadataTargets, metadataSeen = {}, {}, {}, {}
	end
	local fingerprintsByService = {}
	local generationsByService = {}
	if hasStructuralChanges then
		for _, serviceName in ipairs(serviceNames) do
			fingerprintsByService[serviceName] = captureNativeExportFingerprint(serviceName, ctx)
			generationsByService[serviceName] = ctx.studioChangeGeneration(serviceName)
		end
	end
	local payload = nil
	if #roots > 0 then
		local okSerialize, serialized = pcall(SerializationService.SerializeInstancesAsync, SerializationService, roots)
		if not okSerialize then
			error("Cannot create an editor rollback snapshot: " .. tostring(serialized))
		end
		payload = serialized
	end
	for serviceName, fingerprint in pairs(fingerprintsByService) do
		local currentGeneration = ctx.studioChangeGeneration(serviceName)
		if
			not nativeExportFingerprintMatches(fingerprint, captureNativeExportFingerprint(serviceName, ctx))
			or currentGeneration ~= generationsByService[serviceName]
		then
			local attempt = tonumber(params.snapshotAttempt) or 0
			if attempt < 2 then
				local retryParams = table.clone(params)
				retryParams.snapshotAttempt = attempt + 1
				return captureMutationSnapshot(serviceNames, retryParams, ctx)
			end
			error(`Studio kept changing {serviceName} while Renium prepared rollback data`)
		end
	end
	local properties = {}
	local sources = {}
	local sourceKeys = {}
	local propertySeen = {}
	local unreadablePropertyNames = {}
	if not hasStructuralChanges then
		local sourceSeen = {}
		for _, change in ipairs(sourceChanges) do
			local key = pathCacheKey(change.pathSegments, change.pathOrdinals)
			sourceKeys[key] = true
			local instance = resolveInstance(change, ctx, true)
			if instance ~= nil and ctx.luaSourceClass[instance.ClassName] and not sourceSeen[instance] then
				local okRead, source = readScriptSource(instance)
				if not okRead then
					error(`Could not snapshot Source for {instance:GetFullName()}: {source}`)
				end
				sourceSeen[instance] = true
				sources[#sources + 1] = {
					instance = instance,
					source = source,
				}
			end
		end
	end
	for _, change in ipairs(params.propertyChanges or {}) do
		local instance = resolveInstance(change, ctx, true)
		if instance ~= nil and (not hasStructuralChanges or metadataSeen[instance]) then
			addSnapshotMetadataTarget(metadataTargets, metadataSeen, instance)
			local seenNames = propertySeen[instance]
			if seenNames == nil then
				seenNames = {}
				propertySeen[instance] = seenNames
			end
			for propertyName in pairs(change.properties or {}) do
				propertyName = tostring(propertyName)
				if propertyName ~= "Tags" and not seenNames[propertyName] then
					local okRead, value = readProperty(instance, propertyName)
					if okRead then
						seenNames[propertyName] = true
						table.insert(properties, {
							instance = instance,
							name = propertyName,
							value = value,
						})
					else
						local unreadableNames = unreadablePropertyNames[instance]
						if unreadableNames == nil then
							unreadableNames = {}
							unreadablePropertyNames[instance] = unreadableNames
						end
						unreadableNames[propertyName] = true
					end
				end
			end
		end
	end
	local metadata = table.create(#metadataTargets)
	for index, instance in ipairs(metadataTargets) do
		metadata[index] = {
			instance = instance,
			attributes = instance:GetAttributes(),
			tags = CollectionService:GetTags(instance),
		}
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
					local pathSegments, pathOrdinals = BridgeIdentity.getRefPathParts(instance)
					if pathSegments ~= nil then
						originalByPath[pathCacheKey(pathSegments, pathOrdinals)] = instance
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
		sources = sources,
		unreadablePropertyNames = unreadablePropertyNames,
		originalByPath = originalByPath,
		originalRoots = originalRoots,
		currentCamera = Workspace.CurrentCamera,
		scriptDocuments = ScriptDocumentState.capture(
			serviceNames,
			if hasStructuralChanges or params.captureAllScriptDocuments == true then nil else sourceKeys
		),
		referenceOverlay = ReferenceOverlay.capture(groups),
	}
end

local function restoreSnapshotMetadata(
	snapshot: { [string]: any },
	replacements: { [Instance]: Instance },
	ctx: { [string]: any }
)
	for _, entry in ipairs(snapshot.metadata) do
		local instance = replacements[entry.instance] or entry.instance
		local desiredAttributes = entry.attributes
		for name in pairs(instance:GetAttributes()) do
			if desiredAttributes[name] == nil then
				local okWrite, writeError = setAttributeForSync(instance, name, nil, ctx)
				if not okWrite then
					error(`Could not restore {instance:GetFullName()}.{name}: {writeError}`)
				end
			end
		end
		for name, value in pairs(desiredAttributes) do
			if not valuesEqual(instance:GetAttribute(name), value) then
				local okWrite, writeError = setAttributeForSync(instance, name, value, ctx)
				if not okWrite then
					error(`Could not restore {instance:GetFullName()}.{name}: {writeError}`)
				end
			end
		end
		local desiredTags = {}
		for _, tag in ipairs(entry.tags) do
			desiredTags[tag] = true
		end
		for _, tag in ipairs(CollectionService:GetTags(instance)) do
			if not desiredTags[tag] then
				setTagForSync(instance, tag, false, ctx)
			end
		end
		for tag in pairs(desiredTags) do
			if not CollectionService:HasTag(instance, tag) then
				setTagForSync(instance, tag, true, ctx)
			end
		end
	end
	for _, entry in ipairs(snapshot.properties) do
		local instance = replacements[entry.instance] or entry.instance
		local value = if typeof(entry.value) == "Instance"
			then replacements[entry.value] or entry.value
			else entry.value
		local okRead, current = readProperty(instance, entry.name)
		if not okRead or not valuesEqual(current, value) then
			local okWrite, writeError = writePropertyForSync(instance, entry.name, value, ctx)
			if not okWrite then
				error(`Could not restore {instance:GetFullName()}.{entry.name}: {writeError}`)
			end
		end
	end
	for _, entry in ipairs(snapshot.sources or {}) do
		local instance = replacements[entry.instance] or entry.instance
		local okRead, currentSource = readScriptSource(instance)
		if not okRead or currentSource ~= entry.source then
			local okWrite, writeError = setSource(instance, entry.source, ctx)
			if not okWrite then
				error(`Could not restore {instance:GetFullName()}.Source: {writeError}`)
			end
		end
	end
end

local function restoreMutationSnapshot(
	snapshot: { [string]: any },
	ctx: { [string]: any },
	mutationReplacements: { [Instance]: Instance }?,
	beforeReplace: (() -> ())?
): { [Instance]: Instance }
	local roots = if snapshot.payload ~= nil
		then SerializationService:DeserializeInstancesAsync(snapshot.payload)
		else {}
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
	if beforeReplace ~= nil then
		beforeReplace()
	end
	local removed = {}
	local parented = {}
	local okRestore, restoreError = pcall(function()
		for groupIndex, group in ipairs(snapshot.groups) do
			for _, child in ipairs(group.target:GetChildren()) do
				if not group.preserved[child] then
					setParentForSync(child, nil, ctx)
					table.insert(removed, { instance = child, parent = group.target })
				end
			end
			for _, instance in ipairs(incomingByGroup[groupIndex]) do
				setParentForSync(instance, group.target, ctx)
				table.insert(parented, instance)
			end
		end
	end)
	if not okRestore then
		for _, instance in ipairs(parented) do
			setParentForSync(instance, nil, ctx)
		end
		for _, entry in ipairs(removed) do
			setParentForSync(entry.instance, entry.parent, ctx)
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
	if next(replacements) then
		local scanRoots = {}
		for serviceName, allowed in pairs(ctx.allowedServices) do
			if allowed then
				table.insert(scanRoots, game:GetService(serviceName))
			end
		end
		local _, failed = BridgeReferenceRetarget.apply(
			scanRoots,
			replacements,
			RbxDomModule.getReferencePropertyNames,
			readProperty,
			function(instance, propertyName, value)
				return writePropertyForSync(instance, propertyName, value, ctx)
			end
		)
		if failed > 0 then
			error(string.format("Could not restore %d external instance references", failed))
		end
		local _, contentFailed = ReferenceOverlay.retargetPreservedContent(scanRoots, replacements, ctx)
		if contentFailed > 0 then
			error(string.format("Could not restore %d external content references", contentFailed))
		end
	end
	restoreSnapshotMetadata(snapshot, replacements, ctx)
	if snapshot.currentCamera ~= nil then
		setCurrentCameraForSync(replacements[snapshot.currentCamera] or snapshot.currentCamera, ctx)
	end
	ScriptDocumentState.apply(snapshot.scriptDocuments or {}, nil, nil, replacements)
	ReferenceOverlay.apply(snapshot.referenceOverlay or {}, replacements, ctx)
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

local function resolveReplacement(instance: Instance?, replacements: { [Instance]: Instance }): Instance?
	local current = instance
	local seen = {}
	while current ~= nil and replacements[current] ~= nil and not seen[current] do
		seen[current] = true
		current = replacements[current]
	end
	return current
end

local function appendTransactionJournalRecords(session: { [string]: any }, records: { any })
	for _, record in ipairs(records) do
		local instance = resolveReplacement(record.instance, session.instanceReplacements) or record.instance
		record.currentInstance = instance
		record.parent = instance.Parent
		if record.parent ~= nil then
			record.parentPathSegments, record.parentPathOrdinals = BridgeIdentity.getRefPathParts(record.parent)
		end
		if record.tagsChanged then
			local okTags, tags = pcall(CollectionService.GetTags, CollectionService, instance)
			if okTags then
				record.tags = tags
			end
		end
	end
	local journal = session.changeJournal
	if journal == nil then
		journal = {}
		session.changeJournal = journal
	end
	if #records > 0 then
		table.move(records, 1, #records, #journal + 1, journal)
	end
end

local function drainTransactionJournal(session: { [string]: any }, ctx: { [string]: any }): { any }
	if not session.journalActive then
		return {}
	end
	local records = ctx.drainStudioChangeJournal(session.transactionId)
	appendTransactionJournalRecords(session, records)
	return records
end

local function finishTransactionJournal(session: { [string]: any }, ctx: { [string]: any }): { any }
	if session.journalActive then
		local records = ctx.finishStudioChangeJournal(session.transactionId)
		session.journalActive = false
		appendTransactionJournalRecords(session, records)
	end
	return session.changeJournal or {}
end

local function instanceWillBeRemovedByRollback(instance: Instance, session: { [string]: any }): boolean
	for _, group in ipairs(session.snapshot.groups or {}) do
		for _, child in ipairs(group.target:GetChildren()) do
			if not group.preserved[child] and (instance == child or instance:IsDescendantOf(child)) then
				return true
			end
		end
	end
	if session.nativeUndo ~= nil then
		for _, group in ipairs(session.nativeUndo.prepared) do
			for _, root in ipairs(group.incoming) do
				if instance == root or instance:IsDescendantOf(root) then
					return true
				end
			end
		end
	end
	return false
end

local function prepareTransactionJournalRollback(session: { [string]: any }, records: { any }, ctx: { [string]: any })
	local baseline = {}
	for _, instance in pairs(session.snapshot.originalByPath or {}) do
		baseline[instance] = true
	end
	for original, replacement in pairs(session.instanceReplacements) do
		if baseline[original] then
			baseline[replacement] = true
		end
	end
	if session.nativeUndo ~= nil then
		for original, replacement in pairs(session.nativeUndo.replacements) do
			baseline[original] = true
			baseline[replacement] = true
		end
	end
	local candidates = {}
	for _, record in ipairs(records) do
		local instance = record.currentInstance
		if
			instance ~= nil
			and instance.Parent ~= nil
			and not baseline[instance]
			and instanceWillBeRemovedByRollback(instance, session)
		then
			candidates[instance] = true
		end
	end
	local roots = {}
	for instance in pairs(candidates) do
		local ancestor = instance.Parent
		local nested = false
		while ancestor ~= nil do
			if candidates[ancestor] then
				nested = true
				break
			end
			ancestor = ancestor.Parent
		end
		if not nested then
			roots[#roots + 1] = instance
		end
	end
	session.preservedJournalRoots = session.preservedJournalRoots or {}
	for _, root in ipairs(roots) do
		session.preservedJournalRoots[root] = true
		setParentForSync(root, nil, ctx)
	end
end

local function resolveJournalParent(record: { [string]: any }, replacements: { [Instance]: Instance }): Instance?
	local parent = resolveReplacement(record.parent, replacements)
	if parent ~= nil and (parent.Parent ~= nil or parent == game:GetService(record.service)) then
		return parent
	end
	local pathSegments = record.parentPathSegments
	local pathOrdinals = record.parentPathOrdinals
	if type(pathSegments) ~= "table" then
		return nil
	end
	for count = #pathSegments, 1, -1 do
		local segments = table.create(count)
		local ordinals = table.create(count)
		for index = 1, count do
			segments[index] = pathSegments[index]
			ordinals[index] = if type(pathOrdinals) == "table" then pathOrdinals[index] or 1 else 1
		end
		local resolved = resolvePathSegments(segments, nil, ordinals)
		if resolved ~= nil then
			return resolved
		end
	end
	return nil
end

local function replayTransactionJournal(
	records: { any },
	replacements: { [Instance]: Instance },
	ctx: { [string]: any }
)
	for _, record in ipairs(records) do
		local instance = resolveReplacement(record.currentInstance, replacements)
		if instance ~= nil then
			local parent = resolveJournalParent(record, replacements)
			if record.structural or (instance.Parent == nil and parent ~= nil) then
				setParentForSync(instance, parent, ctx)
			end
		end
	end
	for _, record in ipairs(records) do
		local instance = resolveReplacement(record.currentInstance, replacements)
		if instance ~= nil and (instance.Parent ~= nil or instance == game:GetService(record.service)) then
			for propertyName, entry in pairs(record.properties) do
				if entry.captured then
					local value = if typeof(entry.value) == "Instance"
						then resolveReplacement(entry.value, replacements) or entry.value
						else entry.value
					local okWrite, writeError
					if propertyName == "Source" and instance:IsA("LuaSourceContainer") then
						okWrite, writeError = setSource(instance, value, ctx)
					else
						okWrite, writeError = writePropertyForSync(instance, propertyName, value, ctx)
					end
					if not okWrite then
						error(
							`Could not preserve concurrent Studio edit to {instance:GetFullName()}.{propertyName}: {writeError}`
						)
					end
				end
			end
			for attributeName, entry in pairs(record.attributes) do
				if entry.captured then
					local okWrite, writeError = setAttributeForSync(instance, attributeName, entry.value, ctx)
					if not okWrite then
						error(
							`Could not preserve concurrent Studio edit to {instance:GetFullName()}.{attributeName}: {writeError}`
						)
					end
				end
			end
			if record.tags ~= nil then
				local desired = {}
				for _, tag in ipairs(record.tags) do
					desired[tag] = true
				end
				for _, tag in ipairs(CollectionService:GetTags(instance)) do
					if not desired[tag] then
						setTagForSync(instance, tag, false, ctx)
					end
					desired[tag] = nil
				end
				for tag in pairs(desired) do
					setTagForSync(instance, tag, true, ctx)
				end
			end
		end
	end
end

local TransactionState = {}

function TransactionState.rollback(
	session: { [string]: any },
	ctx: { [string]: any },
	beforeReplace: (() -> ())?
): { [Instance]: Instance }
	local incoming = {}
	local replacements = {}
	if session.nativeUndo ~= nil then
		for original, replacement in pairs(session.nativeUndo.replacements) do
			replacements[replacement] = original
		end
		incoming = ReferenceOverlay.rollbackNative(session.nativeUndo, ctx)
	end
	local snapshotReplacements =
		restoreMutationSnapshot(session.snapshot, ctx, session.instanceReplacements, beforeReplace)
	for original, replacement in pairs(snapshotReplacements) do
		replacements[original] = replacement
	end
	for original, replacement in pairs(replacements) do
		replacements[original] = snapshotReplacements[replacement] or replacement
	end
	if beforeReplace ~= nil then
		beforeReplace()
	end
	for _, instance in ipairs(incoming) do
		if not session.preservedJournalRoots or not session.preservedJournalRoots[instance] then
			instance:Destroy()
		end
	end
	return replacements
end

local function rollbackTransactionSession(session: { [string]: any }, ctx: { [string]: any }): { [Instance]: Instance }
	local ok, result = xpcall(function()
		local initialRecords = drainTransactionJournal(session, ctx)
		prepareTransactionJournalRollback(session, initialRecords, ctx)
		local function preservePendingJournalChanges()
			local records = drainTransactionJournal(session, ctx)
			prepareTransactionJournalRollback(session, records, ctx)
		end
		local replacements = TransactionState.rollback(session, ctx, preservePendingJournalChanges)
		local records = finishTransactionJournal(session, ctx)
		replayTransactionJournal(records, replacements, ctx)
		session.changeJournal = nil
		return replacements
	end, debug.traceback)
	if not ok then
		if session.journalActive then
			pcall(ctx.finishStudioChangeJournal, session.transactionId)
			session.journalActive = false
		end
		error(result, 0)
	end
	return result
end

captureNativeExportFingerprint = function(serviceName: string, ctx: { [string]: any }): { any }
	local service = game:GetService(serviceName)
	local fingerprint = {
		{ service, service.Parent, service.Name, 1 },
	}
	local countsByParent = {}
	for _, instance in ipairs(service:GetDescendants()) do
		local parent = instance.Parent
		local counts = countsByParent[parent]
		if counts == nil then
			counts = {}
			countsByParent[parent] = counts
		end
		local ordinal = (counts[instance.Name] or 0) + 1
		counts[instance.Name] = ordinal
		if ctx.includeExportInstance(serviceName, instance) then
			fingerprint[#fingerprint + 1] = {
				instance,
				parent,
				instance.Name,
				ordinal,
			}
		end
	end
	return fingerprint
end

nativeExportFingerprintMatches = function(expected: { any }, actual: { any }): boolean
	if #expected ~= #actual then
		return false
	end
	for index, expectedEntry in ipairs(expected) do
		local actualEntry = actual[index]
		if
			actualEntry == nil
			or expectedEntry[1] ~= actualEntry[1]
			or expectedEntry[2] ~= actualEntry[2]
			or expectedEntry[3] ~= actualEntry[3]
			or expectedEntry[4] ~= actualEntry[4]
		then
			return false
		end
	end
	return true
end

local function validateNativeExportFingerprint(
	session: { [string]: any },
	serviceName: string,
	ctx: { [string]: any },
	state: { [string]: any }?
): boolean
	local expected = session.fingerprintsByService[serviceName]
	if state ~= nil and state.nativeSnapshotRoot then
		if expected == nil or #(state.instances or {}) ~= #expected then
			session.structureChanged = true
			return false
		end
		return true
	end
	if
		expected == nil
		or not nativeExportFingerprintMatches(expected, captureNativeExportFingerprint(serviceName, ctx))
	then
		session.structureChanged = true
		return false
	end
	if state ~= nil then
		local instances = state.instances or {}
		if #instances ~= #expected then
			session.structureChanged = true
			return false
		end
		for index, entry in ipairs(expected) do
			if instances[index] ~= entry[1] then
				session.structureChanged = true
				return false
			end
		end
	end
	return true
end

local function updateNativeSerializationStatus(session: { [string]: any })
	if session.serializationScheduleReady and session.pendingPayloads == 0 then
		session.status = if session.error or next(session.payloadErrors) then "failed" else "ready"
	end
end

local prepareNativeSnapshotSerializationJob

local function appendNativeSerializationJob(
	session: { [string]: any },
	payloadKey: string,
	roots: { Instance },
	markers: { Instance },
	nonArchivableInstances: { Instance }
)
	for _, instance in ipairs(nonArchivableInstances) do
		session.originalNonArchivableInstances[instance] = true
	end
	local job = {
		payloadKey = payloadKey,
		roots = roots,
		markers = markers,
		nonArchivableInstances = nonArchivableInstances,
	}
	if session.snapshotReplacements ~= nil then
		prepareNativeSnapshotSerializationJob(session, job)
	end
	session.serializationJobs[#session.serializationJobs + 1] = job
	session.pendingPayloads += 1
end

local function collectNonArchivableInstances(roots: { Instance }, seen: { [Instance]: boolean }?): { Instance }
	local included = seen or {}
	local instances = {}
	for _, root in ipairs(roots) do
		if not included[root] then
			included[root] = true
			if not root.Archivable then
				instances[#instances + 1] = root
			end
		end
		for _, descendant in ipairs(root:GetDescendants()) do
			if not included[descendant] then
				included[descendant] = true
				if not descendant.Archivable then
					instances[#instances + 1] = descendant
				end
			end
		end
	end
	return instances
end

local function destroyNativeSnapshotRoots(snapshotRoots: { Instance }, replacements: { [Instance]: Instance })
	for index = 2, #snapshotRoots do
		local root = snapshotRoots[index]
		if replacements[root] ~= root and not root:IsA("Terrain") then
			root:Destroy()
		end
	end
end

local function captureNativeExportSnapshot(
	serviceName: string,
	roots: { Instance },
	fingerprint: { any },
	ctx: { [string]: any }
): ({ Instance }, { [string]: any }, { [Instance]: Instance }, { Instance })
	local generationBefore = ctx.studioChangeGeneration(serviceName)
	local marker = roots[1]
	local service = fingerprint[1][1]
	local rootPropertyValues = ctx.captureRootProperties(serviceName)
	for name, value in pairs(service:GetAttributes()) do
		if name:sub(1, 3) ~= "RBX" then
			marker:SetAttribute(name, value)
		end
	end
	for _, tag in ipairs(CollectionService:GetTags(service)) do
		CollectionService:AddTag(marker, tag)
	end
	local scriptDocuments = ScriptDocumentState.capture({ serviceName })
	for index, root in ipairs(roots) do
		if index > 1 and containsPackageLink(root) then
			if #collectNonArchivableInstances({ root }) > 0 then
				error(`Package root {root:GetFullName()} contains a non-archivable instance`)
			end
		end
	end
	local changed = collectNonArchivableInstances(roots)
	local snapshotRoots = { roots[1] }
	local replacements = { [service] = marker }
	local function mapCloneTree(original: Instance, clone: Instance)
		local pending = { { original, clone } }
		while #pending > 0 do
			local pair = table.remove(pending)
			local currentOriginal = pair[1]
			local currentClone = pair[2]
			replacements[currentOriginal] = currentClone
			local originalChildren = currentOriginal:GetChildren()
			local cloneChildren = currentClone:GetChildren()
			if #originalChildren ~= #cloneChildren then
				error("Native export clone did not preserve the instance tree")
			end
			for index = #originalChildren, 1, -1 do
				pending[#pending + 1] = { originalChildren[index], cloneChildren[index] }
			end
		end
	end
	ctx.beginStudioChangeSuppression(0)
	local okClone, cloneError = xpcall(function()
		for _, instance in ipairs(changed) do
			if instance.Parent ~= nil and not instance.Archivable then
				local okWrite, writeError = writePropertyForSync(instance, "Archivable", true, ctx)
				if not okWrite then
					error(writeError, 0)
				end
			end
		end
		for index = 2, #roots do
			local root = roots[index]
			if root:IsA("Terrain") or root:IsA("StarterPlayerScripts") or root:IsA("StarterCharacterScripts") then
				local serializedRoot = SerializationService:SerializeInstancesAsync({ root })
				local clone = SerializationService:DeserializeInstancesAsync(serializedRoot)[1]
				if clone == nil or clone.ClassName ~= root.ClassName then
					error("Native export could not copy " .. root.ClassName)
				end
				snapshotRoots[index] = clone
				mapCloneTree(root, clone)
			else
				local clone = root:Clone()
				snapshotRoots[index] = clone
				mapCloneTree(root, clone)
			end
		end
		for _, entry in ipairs(scriptDocuments) do
			local clone = replacements[entry.instance]
			if clone ~= nil then
				local okSource, sourceError = setSource(clone, entry.source, nil)
				if not okSource then
					error(sourceError, 0)
				end
			end
		end
	end, debug.traceback)
	local restoreError = nil
	for index = #changed, 1, -1 do
		local instance = changed[index]
		local restored, result = pcall(function()
			local okWrite, writeError = writePropertyForSync(instance, "Archivable", false, ctx)
			if not okWrite then
				error(writeError, 0)
			end
		end)
		if not restored and restoreError == nil then
			restoreError = result
		end
	end
	ctx.endStudioChangeSuppression(0)
	if not okClone or restoreError ~= nil then
		destroyNativeSnapshotRoots(snapshotRoots, replacements)
		error(restoreError or cloneError, 0)
	end
	local instances = table.create(#fingerprint)
	local debugIds = table.create(#fingerprint)
	local debugIdByInstance = {}
	local pathByInstance = {}
	local pathSegmentsByInstance = {}
	local pathOrdinalsByInstance = {}
	local nonArchivableClones = {}
	local okSnapshot, snapshotError = xpcall(function()
		for index, entry in ipairs(fingerprint) do
			local original = entry[1]
			local clone = if index == 1 then snapshotRoots[1] else replacements[original]
			if clone == nil then
				error("Native export clone omitted an included instance")
			end
			local debugId = original:GetDebugId(32)
			instances[index] = clone
			debugIds[index] = debugId
			debugIdByInstance[original] = debugId
			debugIdByInstance[clone] = debugId
			local parentSegments = if index == 1 then {} else pathSegmentsByInstance[entry[2]]
			local parentOrdinals = if index == 1 then {} else pathOrdinalsByInstance[entry[2]]
			if parentSegments == nil or parentOrdinals == nil then
				error("Native export fingerprint did not preserve parent order")
			end
			local pathSegments = table.clone(parentSegments)
			local pathOrdinals = table.clone(parentOrdinals)
			pathSegments[#pathSegments + 1] = entry[3]
			pathOrdinals[#pathOrdinals + 1] = entry[4]
			local path = table.concat(pathSegments, ".")
			pathByInstance[original] = path
			pathByInstance[clone] = path
			pathSegmentsByInstance[original] = pathSegments
			pathSegmentsByInstance[clone] = pathSegments
			pathOrdinalsByInstance[original] = pathOrdinals
			pathOrdinalsByInstance[clone] = pathOrdinals
			if not original.Archivable then
				nonArchivableClones[#nonArchivableClones + 1] = clone
			end
		end
		local generationAfter = ctx.studioChangeGeneration(serviceName)
		if
			(generationBefore ~= nil and generationAfter ~= generationBefore)
			or not nativeExportFingerprintMatches(fingerprint, captureNativeExportFingerprint(serviceName, ctx))
		then
			error("Studio changed during native export snapshot capture")
		end
	end, debug.traceback)
	if not okSnapshot then
		destroyNativeSnapshotRoots(snapshotRoots, replacements)
		error(snapshotError, 0)
	end
	return snapshotRoots,
		{
			serviceName = serviceName,
			serviceClassName = fingerprint[1][1].ClassName,
			instances = instances,
			debugIds = debugIds,
			debugIdBuffer = buffer.fromstring(table.concat(debugIds, "\0")),
			debugIdByInstance = debugIdByInstance,
			pathByInstance = pathByInstance,
			pathSegmentsByInstance = pathSegmentsByInstance,
			pathOrdinalsByInstance = pathOrdinalsByInstance,
			rootPropertyValues = rootPropertyValues,
		},
		replacements,
		nonArchivableClones
end

prepareNativeSnapshotSerializationJob = function(session: { [string]: any }, job: { [string]: any })
	local _, referenceFailures = BridgeReferenceRetarget.apply(
		job.roots,
		session.snapshotReplacements,
		RbxDomModule.getReferencePropertyNames,
		readProperty,
		writeProperty
	)
	local _, contentFailures = ReferenceOverlay.retargetPreservedContent(job.roots, session.snapshotReplacements)
	if referenceFailures > 0 or contentFailures > 0 then
		error(`Could not preserve {referenceFailures + contentFailures} cross-root references in native export`)
	end
end

local function appendNativeSerializationGroups(
	session: { [string]: any },
	groups: { any },
	rootsByService: { [string]: { Instance } }
)
	if #groups == 1 then
		local group = groups[1]
		local roots = rootsByService[group.service]
		appendNativeSerializationJob(session, group.service, roots, { roots[1] }, collectNonArchivableInstances(roots))
		return
	end

	local roots = {}
	local markers = table.create(#groups)
	local services = table.create(#groups)
	for index, group in ipairs(groups) do
		local groupRoots = rootsByService[group.service]
		services[index] = group.service
		markers[index] = groupRoots[1]
		for _, root in ipairs(groupRoots) do
			roots[#roots + 1] = root
		end
	end
	local batchId = "native-batch-" .. tostring(#session.serializationBatches + 1)
	session.serializationBatches[#session.serializationBatches + 1] = {
		id = batchId,
		services = services,
	}
	session.serializationBatchIds[batchId] = true
	appendNativeSerializationJob(session, batchId, roots, markers, collectNonArchivableInstances(roots))
end

local function finishNativeSerializationSchedule(
	session: { [string]: any },
	groups: { any },
	rootsByService: { [string]: { Instance } },
	startIndex: number
)
	local pendingGroups = {}
	local pendingInstanceCount = 0
	local function flushPendingGroups()
		if #pendingGroups > 0 then
			appendNativeSerializationGroups(session, pendingGroups, rootsByService)
		end
		pendingGroups = {}
		pendingInstanceCount = 0
	end
	for index = startIndex, #groups do
		local group = groups[index]
		if group.instanceCount < NATIVE_SERIALIZATION_SERVICE_LIMIT then
			if pendingInstanceCount + group.instanceCount > NATIVE_SERIALIZATION_BATCH_LIMIT then
				flushPendingGroups()
			end
			pendingGroups[#pendingGroups + 1] = group
			pendingInstanceCount += group.instanceCount
		else
			flushPendingGroups()
			local roots = rootsByService[group.service]
			appendNativeSerializationJob(
				session,
				group.service,
				roots,
				{ roots[1] },
				collectNonArchivableInstances(roots)
			)
		end
	end
	flushPendingGroups()
	session.serializationScheduleReady = true
	updateNativeSerializationStatus(session)
	session.payloadReadyEvent:Fire()
end

function BridgeEditorSync.create(ctx: { [string]: any })
	local api = {}
	local cancellationGeneration = 0
	local filterCandidateSnapshots = {}
	local filterCandidateSnapshotCounter = 0
	local filterCandidateSnapshotTtlSeconds = 15
	local maxFilterCandidateSnapshots = 8

	local function pruneFilterCandidateSnapshots()
		local now = os.clock()
		local retained = {}
		for id, snapshot in pairs(filterCandidateSnapshots) do
			if snapshot.expiresAt <= now then
				filterCandidateSnapshots[id] = nil
			else
				retained[#retained + 1] = snapshot
			end
		end
		table.sort(retained, function(a, b)
			return a.createdAt < b.createdAt
		end)
		for index = 1, math.max(0, #retained - maxFilterCandidateSnapshots) do
			filterCandidateSnapshots[retained[index].id] = nil
		end
	end
	api.stats = ctx.stats

	function api.getLiveSourceBatch(params: { [string]: any }): { [string]: any }
		local selectors = params.selectors
		local dense, count = denseArrayLength(selectors)
		if not dense or count > ctx.maxChangesPerRequest then
			error("Live source batch is invalid")
		end
		local rows = table.create(count)
		for position, selector in ipairs(selectors) do
			local index = tonumber(selector.index) or position
			local instance = resolvePathSegments(selector.pathSegments, nil, selector.pathOrdinals)
			if instance == nil or not ctx.luaSourceClass[instance.ClassName] then
				rows[position] = { index = index, error = "Script was not found" }
			else
				local ok, source = readScriptSource(instance)
				if ok and type(source) == "string" then
					rows[position] = { index = index, source = source }
				else
					rows[position] = { index = index, error = tostring(source) }
				end
			end
		end
		return { rows = rows }
	end

	local function assertSessionOwnership(operationGeneration: number?)
		if operationGeneration ~= nil and operationGeneration ~= cancellationGeneration then
			error("Renium operation was cancelled")
		end
		ctx.assertSessionOwnership()
	end

	local function runWithSessionOwnership(operationGeneration, cancellationCheck, operation, ...)
		assertSessionOwnership(operationGeneration)
		cancellationCheck()
		local results = table.pack(pcall(operation, ...))
		cancellationCheck()
		assertSessionOwnership(operationGeneration)
		if not results[1] then
			error(results[2], 0)
		end
		return table.unpack(results, 2, results.n)
	end

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

	function api.getServiceChangeGenerations(params: { [string]: any }): { [string]: any }
		local servicesAreArray, serviceCount = denseArrayLength(params.services)
		if not servicesAreArray or serviceCount < 1 then
			error("Invalid service generation request")
		end
		local generations = {}
		local hasPackageLinks = {}
		for _, rawServiceName in ipairs(params.services) do
			local serviceName = tostring(rawServiceName)
			if not ctx.allowedServices[serviceName] or generations[serviceName] ~= nil then
				error("Invalid service generation request")
			end
			generations[serviceName] = ctx.studioChangeGeneration(serviceName)
			hasPackageLinks[serviceName] = game:GetService(serviceName):FindFirstChildWhichIsA("PackageLink", true)
				~= nil
		end
		return { generations = generations, hasPackageLinks = hasPackageLinks }
	end

	function api.beginTransaction(params: { [string]: any }): { [string]: any }
		pruneExpiredSessions(editorTransactions)
		local transactionId = tostring(params.transactionId or "")
		local servicesAreArray, serviceCount = denseArrayLength(params.services)
		if transactionId == "" or not servicesAreArray or serviceCount < 1 then
			error("Invalid editor transaction")
		end
		if editorTransactions[transactionId] ~= nil or countEntries(editorTransactions) > 0 then
			error("Another editor transaction is already active")
		end
		local serviceNames = table.create(serviceCount)
		local includedServices = {}
		for index, rawServiceName in ipairs(params.services) do
			local serviceName = tostring(rawServiceName)
			if not ctx.allowedServices[serviceName] or includedServices[serviceName] then
				error("Invalid editor transaction service")
			end
			includedServices[serviceName] = true
			serviceNames[index] = serviceName
		end
		local changedSourceKeys = {}
		local changedSourceInstances = {}
		for _, change in ipairs(params.sourceChanges or {}) do
			local pathSegments = change.pathSegments
			if type(pathSegments) ~= "table" or not includedServices[tostring(change.service or "")] then
				error("Invalid editor transaction source")
			end
			changedSourceKeys[pathCacheKey(pathSegments, change.pathOrdinals)] = true
			local changedInstance = resolvePathSegments(pathSegments, nil, change.pathOrdinals)
			if changedInstance ~= nil and changedInstance:IsA("LuaSourceContainer") then
				changedSourceInstances[changedInstance] = true
			end
		end
		local nativeImport = params.nativeImport == true
		local snapshot = captureMutationSnapshot(serviceNames, {
			forceStructural = not nativeImport and params.hasInstanceChanges == true,
			skipStructural = nativeImport,
			captureAllScriptDocuments = nativeImport,
			instanceChanges = {},
			sourceChanges = params.sourceChanges or {},
			propertyChanges = params.propertyChanges or {},
		}, ctx)
		local session = {
			transactionId = transactionId,
			serviceNames = serviceNames,
			changedSourceKeys = changedSourceKeys,
			changedSourceInstances = changedSourceInstances,
			instanceReplacements = {},
			snapshot = snapshot,
			nativeImport = nativeImport,
			historyRecording = beginHistoryRecording("Sync from filesystem"),
			journalActive = false,
		}
		local okJournal, journalError = pcall(ctx.beginStudioChangeJournal, transactionId, serviceNames)
		if not okJournal then
			finishHistoryRecording(session.historyRecording, Enum.FinishRecordingOperation.Cancel)
			error(journalError, 0)
		end
		session.journalActive = true
		session.onExpire = function()
			local ok, result = pcall(runWithStudioChangeSuppression, ctx, function()
				return rollbackTransactionSession(session, ctx)
			end)
			finishHistoryRecording(session.historyRecording, Enum.FinishRecordingOperation.Cancel)
			session.historyRecording = nil
			if not ok then
				error(result, 0)
			end
		end
		editorTransactions[transactionId] = session
		armSessionExpiry(editorTransactions, transactionId, session)
		return { ok = true, transactionId = transactionId }
	end

	function api.commitTransaction(params: { [string]: any }): { [string]: any }
		pruneExpiredSessions(editorTransactions)
		local profile = params.profile == true
		local timings = {}
		local phaseStarted = os.clock()
		local transactionId = tostring(params.transactionId or "")
		local session = editorTransactions[transactionId]
		if type(session) ~= "table" then
			error("Editor transaction was not found")
		end
		if session.nativeUndo ~= nil then
			ReferenceOverlay.chainReplacements(session.instanceReplacements, session.nativeUndo.replacements)
			drainTransactionJournal(session, ctx)
			ReferenceOverlay.commitNative(session.nativeUndo, ctx)
			for original, replacement in pairs(session.nativeUndo.replacements) do
				session.instanceReplacements[original] = replacement
			end
		end
		timings.nativeCommitMs = (os.clock() - phaseStarted) * 1000
		phaseStarted = os.clock()
		ScriptDocumentState.apply(
			session.snapshot.scriptDocuments or {},
			session.changedSourceInstances,
			session.changedSourceKeys,
			session.instanceReplacements,
			session.resolveStagedPath,
			session.nativeImport
		)
		timings.scriptDocumentsMs = (os.clock() - phaseStarted) * 1000
		phaseStarted = os.clock()
		local records = finishTransactionJournal(session, ctx)
		replayTransactionJournal(records, session.instanceReplacements, ctx)
		timings.journalMs = (os.clock() - phaseStarted) * 1000
		phaseStarted = os.clock()
		finishHistoryRecording(session.historyRecording)
		timings.historyMs = (os.clock() - phaseStarted) * 1000
		phaseStarted = os.clock()
		local undoRecorded = session.historyRecording ~= nil
		session.historyRecording = nil
		if session.nativeStats ~= nil then
			ctx.stats.requests += session.nativeStats.requests
			ctx.stats.lastMs = session.nativeStats.lastMs
			ctx.stats.lastAtUnix = os.time()
			ctx.stats.lastOk = true
			ctx.stats.instanceCreated += session.nativeStats.instanceCreated
			ctx.updateStatus()
		end
		session.onExpire = nil
		session.changeJournal = nil
		if session.nativeUndo ~= nil then
			for _, group in ipairs(session.nativeUndo.prepared) do
				for _, instance in ipairs(group.outgoing) do
					if instance.Parent == nil then
						instance:Destroy()
					end
				end
			end
			for _, instance in ipairs(session.nativeUndo.retainedDuplicates or {}) do
				instance:Destroy()
			end
			session.nativeUndo = nil
			session.resolveStagedPath = nil
		end
		timings.cleanupMs = (os.clock() - phaseStarted) * 1000
		editorTransactions[transactionId] = nil
		return {
			ok = true,
			transactionId = transactionId,
			undoRecorded = undoRecorded,
			profile = if profile then timings else nil,
		}
	end

	function api.rollbackTransaction(params: { [string]: any }): { [string]: any }
		local transactionId = tostring(params.transactionId or "")
		local session = editorTransactions[transactionId]
		if type(session) ~= "table" then
			return { ok = true, found = false }
		end
		session.onExpire = nil
		editorTransactions[transactionId] = nil
		local ok, replacements = pcall(runWithStudioChangeSuppression, ctx, function()
			return rollbackTransactionSession(session, ctx)
		end)
		finishHistoryRecording(session.historyRecording, Enum.FinishRecordingOperation.Cancel)
		session.historyRecording = nil
		if not ok then
			error(replacements, 0)
		end
		for _, serviceName in ipairs(session.serviceNames) do
			ctx.invalidateService(serviceName)
		end
		return {
			ok = true,
			found = true,
			replacements = countEntries(replacements),
		}
	end

	function api.beginBinaryExport(params: { [string]: any }): { [string]: any }
		pruneExpiredSessions(binaryExports)
		local exportId = tostring(params.exportId or "")
		local partitioned = params.partitioned == true
		local metadataOnly = params.metadataOnly == true
		if exportId == "" then
			error("Invalid native export id")
		end
		for key, previous in pairs(binaryExports) do
			binaryExports[key] = nil
			if type(previous) == "table" and type(previous.onExpire) == "function" then
				local okExpire, expireError = pcall(previous.onExpire)
				if not okExpire then
					warn("[Renium] native export cleanup failed: " .. tostring(expireError))
				end
			end
		end
		local serviceNames = {}
		for serviceName, allowed in pairs(ctx.allowedServices) do
			if allowed then
				serviceNames[#serviceNames + 1] = serviceName
			end
		end
		table.sort(serviceNames)
		if partitioned and type(params.serviceOrder) == "table" then
			local allowedByName = {}
			for _, serviceName in ipairs(serviceNames) do
				allowedByName[serviceName] = true
			end
			local orderedNames = {}
			local included = {}
			for _, rawServiceName in ipairs(params.serviceOrder) do
				local serviceName = tostring(rawServiceName)
				if allowedByName[serviceName] and not included[serviceName] then
					included[serviceName] = true
					orderedNames[#orderedNames + 1] = serviceName
				end
			end
			for _, serviceName in ipairs(serviceNames) do
				if not included[serviceName] then
					orderedNames[#orderedNames + 1] = serviceName
				end
			end
			serviceNames = orderedNames
		end
		local roots = {}
		local markers = {}
		local groups = {}
		local groupByService = {}
		local rootsByService = {}
		local fingerprintsByService = {}
		local function failChangedExport()
			for _, marker in ipairs(markers) do
				marker:Destroy()
			end
			error("Studio structure changed during native export")
		end
		for _, serviceName in ipairs(serviceNames) do
			local fingerprintBefore = captureNativeExportFingerprint(serviceName, ctx)
			local service = game:GetService(serviceName)
			local marker = Instance.new("Folder")
			marker.Name = serviceName
			markers[#markers + 1] = marker
			local children = service:GetChildren()
			local groupRoots = table.create(#children + 1)
			groupRoots[1] = marker
			for _, child in ipairs(children) do
				if ctx.includeExportInstance(serviceName, child) then
					groupRoots[#groupRoots + 1] = child
				end
			end
			rootsByService[serviceName] = groupRoots
			if not partitioned then
				for _, root in ipairs(groupRoots) do
					roots[#roots + 1] = root
				end
			end
			local group = {
				service = serviceName,
				targetPath = { serviceName },
				count = #groupRoots - 1,
				changeGeneration = ctx.studioChangeGeneration(serviceName),
			}
			groups[#groups + 1] = group
			groupByService[serviceName] = group
			local fingerprintAfter = captureNativeExportFingerprint(serviceName, ctx)
			if not nativeExportFingerprintMatches(fingerprintBefore, fingerprintAfter) then
				failChangedExport()
			end
			fingerprintsByService[serviceName] = fingerprintAfter
		end
		for _, serviceName in ipairs(serviceNames) do
			if
				not nativeExportFingerprintMatches(
					fingerprintsByService[serviceName],
					captureNativeExportFingerprint(serviceName, ctx)
				)
			then
				failChangedExport()
			end
		end
		local session: { [string]: any } = {
			groups = groups,
			serviceNames = serviceNames,
			nativeStates = {},
			partitioned = partitioned,
			status = "pending",
			structureChanged = false,
			payloads = {},
			payloadErrors = {},
			binaryBatchPayloads = {},
			payloadReadyEvent = Instance.new("BindableEvent"),
			serializationJobs = {},
			serializationBatches = {},
			serializationBatchIds = {},
			originalNonArchivableInstances = {},
			snapshotReplacements = {},
			snapshotPathByInstance = {},
			snapshotPathSegmentsByInstance = {},
			snapshotPathOrdinalsByInstance = {},
			snapshotDebugIdByInstance = {},
			snapshotRoots = table.clone(markers),
			nativeSnapshots = {},
			activeSerializations = 0,
			fingerprintsByService = fingerprintsByService,
			serializationScheduleReady = not partitioned,
			pendingPayloads = if partitioned then 0 else 1,
			updatedAt = os.clock(),
		}
		session.cleanupSnapshot = function()
			if session.activeSerializations > 0 then
				session.snapshotCleanupPending = true
				return
			end
			session.snapshotCleanupPending = false
			for _, root in ipairs(session.snapshotRoots) do
				if not root:IsA("Terrain") then
					root:Destroy()
				end
			end
			table.clear(session.snapshotRoots)
			table.clear(session.snapshotReplacements)
			table.clear(session.snapshotPathByInstance)
			table.clear(session.snapshotPathSegmentsByInstance)
			table.clear(session.snapshotPathOrdinalsByInstance)
			table.clear(session.snapshotDebugIdByInstance)
			table.clear(session.nativeSnapshots)
		end
		session.onExpire = function()
			if session.released then
				return
			end
			session.cancelled = true
			session.released = true
			session.serializationScheduleReady = true
			session.payloadReadyEvent:Fire()
			session.cleanupSnapshot()
			table.clear(session.nativeStates)
			table.clear(session.binaryBatchPayloads)
			session.payloadReadyEvent:Destroy()
		end
		binaryExports[exportId] = session
		armSessionExpiry(binaryExports, exportId, session)
		local beginOk, beginResult = xpcall(function()
			if not metadataOnly then
				for _, serviceName in ipairs(serviceNames) do
					local snapshotRoots, snapshot, replacements, nonArchivableClones = captureNativeExportSnapshot(
						serviceName,
						rootsByService[serviceName],
						fingerprintsByService[serviceName],
						ctx
					)
					rootsByService[serviceName] = snapshotRoots
					session.nativeSnapshots[serviceName] = snapshot
					for index = 2, #snapshotRoots do
						local root = snapshotRoots[index]
						if replacements[root] ~= root then
							session.snapshotRoots[#session.snapshotRoots + 1] = root
						end
					end
					for original, clone in pairs(replacements) do
						session.snapshotReplacements[original] = clone
					end
					for instance, path in pairs(snapshot.pathByInstance) do
						session.snapshotPathByInstance[instance] = path
					end
					for instance, pathSegments in pairs(snapshot.pathSegmentsByInstance) do
						session.snapshotPathSegmentsByInstance[instance] = pathSegments
					end
					for instance, pathOrdinals in pairs(snapshot.pathOrdinalsByInstance) do
						session.snapshotPathOrdinalsByInstance[instance] = pathOrdinals
					end
					for instance, debugId in pairs(snapshot.debugIdByInstance) do
						session.snapshotDebugIdByInstance[instance] = debugId
					end
					for _, instance in ipairs(nonArchivableClones) do
						session.originalNonArchivableInstances[instance] = true
					end
				end
				for _, serviceName in ipairs(serviceNames) do
					local snapshot = session.nativeSnapshots[serviceName]
					snapshot.pathByInstance = session.snapshotPathByInstance
					snapshot.pathSegmentsByInstance = session.snapshotPathSegmentsByInstance
					snapshot.pathOrdinalsByInstance = session.snapshotPathOrdinalsByInstance
					snapshot.debugIdByInstance = session.snapshotDebugIdByInstance
					for propertyName, value in pairs(snapshot.rootPropertyValues) do
						if typeof(value) == "Instance" then
							snapshot.rootPropertyValues[propertyName] = session.snapshotReplacements[value] or value
						end
					end
					local state = ctx.prepareNativeState(serviceName, snapshot)
					if not validateNativeExportFingerprint(session, serviceName, ctx, state) then
						error("Studio structure changed during native export")
					end
					session.nativeStates[serviceName] = state
					local values = ctx.readRootProperties(serviceName, state)
					if type(values) == "table" and next(values) then
						groupByService[serviceName].rootProperties = values
					end
				end
				if not partitioned then
					table.clear(roots)
					for _, serviceName in ipairs(serviceNames) do
						for _, root in ipairs(rootsByService[serviceName]) do
							roots[#roots + 1] = root
						end
					end
					prepareNativeSnapshotSerializationJob(session, { roots = roots })
				end
				local function completePayload(
					payloadKey: string?,
					payloadMarkers: { Instance },
					ok: boolean,
					payload: any
				)
					for _, marker in ipairs(payloadMarkers) do
						marker:Destroy()
					end
					if session.cancelled then
						return
					end
					if not ok then
						if payloadKey then
							session.payloadErrors[payloadKey] = tostring(payload)
						else
							session.error = tostring(payload)
						end
					else
						local totalBytes = buffer.len(payload)
						if totalBytes > 536870912 then
							local message = "Native export exceeds the supported size"
							if payloadKey then
								session.payloadErrors[payloadKey] = message
							else
								session.error = message
							end
						elseif payloadKey then
							session.payloads[payloadKey] = payload
						else
							session.payload = payload
							session.totalBytes = totalBytes
						end
					end
					session.pendingPayloads -= 1
					updateNativeSerializationStatus(session)
					session.updatedAt = os.clock()
					session.payloadReadyEvent:Fire()
				end
				local function runSerializationJob(job: { [string]: any })
					if session.cancelled then
						return
					end
					session.activeSerializations += 1
					local ok, payload = xpcall(function()
						return SerializationService:SerializeInstancesAsync(job.roots)
					end, debug.traceback)
					session.activeSerializations -= 1
					completePayload(job.payloadKey, job.markers, ok, payload)
					if session.snapshotCleanupPending and session.activeSerializations == 0 then
						session.cleanupSnapshot()
					end
				end
				if partitioned then
					local serializerWorkerCount =
						math.clamp(math.floor(tonumber(params.serializationWorkers) or #groups), 1, #groups)
					for index = 1, serializerWorkerCount do
						local group = groups[index]
						local groupRoots = rootsByService[group.service]
						appendNativeSerializationJob(
							session,
							group.service,
							groupRoots,
							{ groupRoots[1] },
							collectNonArchivableInstances(groupRoots)
						)
					end
					session.firstUnscheduledSerializationGroup = serializerWorkerCount + 1
					local nextSerializationIndex = 0
					for _ = 1, serializerWorkerCount do
						task.spawn(function()
							while not session.cancelled do
								local job = session.serializationJobs[nextSerializationIndex + 1]
								if job ~= nil then
									nextSerializationIndex += 1
									runSerializationJob(job)
								elseif session.serializationScheduleReady then
									break
								else
									session.payloadReadyEvent.Event:Wait()
								end
							end
						end)
					end
					session.serializerWorkerCount = serializerWorkerCount
				else
					task.spawn(function()
						runSerializationJob({
							roots = roots,
							markers = markers,
						})
					end)
				end
			else
				session.pendingPayloads = 0
				session.serializationScheduleReady = true
				session.status = "ready"
			end
			local propertySchemaByClass = {}
			local enumValueNamesByType = {}
			for _, serviceName in ipairs(serviceNames) do
				local state = session.nativeStates[serviceName]
				if state == nil then
					state = ctx.getState(serviceName)
					if not validateNativeExportFingerprint(session, serviceName, ctx, state) then
						binaryExports[exportId] = nil
						session.onExpire()
						error("Studio structure changed during native export")
					end
					session.nativeStates[serviceName] = state
				end
				state.originalNonArchivableInstances = session.originalNonArchivableInstances
				local instances = state.instances or {}
				local group = groupByService[serviceName]
				if group.rootProperties == nil then
					local values = ctx.readRootProperties(serviceName, state)
					if type(values) == "table" and next(values) then
						group.rootProperties = values
					end
				end
				group.instanceCount = #instances
				group.classNames = state.classNames or {}
				for _, className in ipairs(state.classNames or {}) do
					if propertySchemaByClass[className] == nil then
						local schema = ctx.getPropertySchema(className)
						propertySchemaByClass[className] = schema
						for _, entry in ipairs(schema) do
							local enumType = entry[3]
							if type(enumType) == "string" and enumValueNamesByType[enumType] == nil then
								local names = ctx.getEnumValueNames(enumType)
								if type(names) == "table" then
									enumValueNamesByType[enumType] = names
								end
							end
						end
					end
				end
			end
			if partitioned then
				finishNativeSerializationSchedule(
					session,
					groups,
					rootsByService,
					session.firstUnscheduledSerializationGroup
				)
			end
			return {
				ok = true,
				exportId = exportId,
				groups = groups,
				serializationBatches = session.serializationBatches,
				propertySchemaByClass = propertySchemaByClass,
				enumValueNamesByType = enumValueNamesByType,
				pending = session.status == "pending",
				supported = true,
			}
		end, debug.traceback)
		if not beginOk then
			binaryExports[exportId] = nil
			session.onExpire()
			error(beginResult, 0)
		end
		return beginResult
	end

	function api.getBinaryExportState(exportId: string, serviceName: string): any
		local session = binaryExports[exportId]
		if type(session) ~= "table" then
			error("Native export session was not found")
		end
		local state = session.nativeStates and session.nativeStates[serviceName]
		if state == nil then
			error("Native export service state was not found")
		end
		if not validateNativeExportFingerprint(session, serviceName, ctx, state) then
			error("Studio structure changed during native export")
		end
		return state
	end

	function api.validateBinaryExportState(exportId: string, serviceName: string)
		api.getBinaryExportState(exportId, serviceName)
	end

	local function binaryExportSession(params: { [string]: any }, requirePartitioned: boolean?): (string, string, any)
		pruneExpiredSessions(binaryExports)
		local exportId = tostring(params.exportId or "")
		local session = binaryExports[exportId]
		if type(session) ~= "table" or requirePartitioned and not session.partitioned then
			error(
				if requirePartitioned
					then "Partitioned native export session was not found"
					else "Native export session was not found"
			)
		end
		armSessionExpiry(binaryExports, exportId, session)
		return exportId, tostring(params.service or ""), session
	end

	function api.awaitBinaryExport(params: { [string]: any }): { [string]: any }
		local exportId, serviceName, session = binaryExportSession(params)
		return runSessionOperation(binaryExports, exportId, session, function()
			if session.structureChanged then
				error("Studio structure changed during native export")
			end
			local timeoutSeconds = math.clamp(tonumber(params.timeoutSeconds) or 30, 1, 120)
			local deadline = os.clock() + timeoutSeconds
			local partitionedService = session.partitioned and serviceName ~= ""
			if
				partitionedService
				and session.nativeStates[serviceName] == nil
				and not session.serializationBatchIds[serviceName]
			then
				error("Native export service was not found")
			end
			local timeoutThread = task.delay(timeoutSeconds, function()
				if binaryExports[exportId] == session then
					session.payloadReadyEvent:Fire()
				end
			end)
			if partitionedService then
				while
					session.payloads[serviceName] == nil
					and session.payloadErrors[serviceName] == nil
					and not session.cancelled
					and os.clock() < deadline
				do
					session.payloadReadyEvent.Event:Wait()
				end
			else
				while session.status == "pending" and not session.cancelled and os.clock() < deadline do
					session.payloadReadyEvent.Event:Wait()
				end
			end
			if coroutine.status(timeoutThread) ~= "dead" then
				task.cancel(timeoutThread)
			end
			if session.cancelled then
				error("Native export was cancelled")
			end
			if
				(
					partitionedService
					and session.payloads[serviceName] == nil
					and session.payloadErrors[serviceName] == nil
				) or (not partitionedService and session.status == "pending")
			then
				error("Native export serialization timed out")
			end
			if session.structureChanged then
				error("Studio structure changed during native export")
			end
			if partitionedService then
				local payloadError = session.payloadErrors[serviceName]
				if payloadError then
					error(payloadError, 0)
				end
				local payload = session.payloads[serviceName]
				return {
					ok = true,
					exportId = exportId,
					service = serviceName,
					totalBytes = buffer.len(payload),
				}
			end
			if session.status ~= "ready" then
				error(session.error or "Native export serialization failed", 0)
			end
			return {
				ok = true,
				exportId = exportId,
				totalBytes = session.totalBytes,
			}
		end)
	end

	function api.readBinaryExport(params: { [string]: any }): { [string]: any }
		local exportId, serviceName, session = binaryExportSession(params)
		return runSessionOperation(binaryExports, exportId, session, function()
			if session.structureChanged then
				error("Studio structure changed during native export")
			end
			if params.waitForReady == true then
				api.awaitBinaryExport(params)
			end
			local payload = session.payload
			local totalBytes = session.totalBytes
			if session.partitioned then
				if serviceName == "" then
					error("Native export service is required")
				end
				local payloadError = session.payloadErrors[serviceName]
				if payloadError then
					error(payloadError, 0)
				end
				payload = session.payloads[serviceName]
				totalBytes = if payload then buffer.len(payload) else nil
			end
			if not payload or not totalBytes then
				error("Native export serialization is not ready")
			end
			local offset, length = binaryReadRange(params, totalBytes, "Native export")
			local chunk = if offset == 0 and length == totalBytes then payload else buffer.create(length)
			if chunk ~= payload then
				buffer.copy(chunk, 0, payload, offset, length)
			end
			if params.rawBase64 == true then
				return encodeBinaryChunk(chunk, offset, length, totalBytes, session.status == "ready")
			end
			return {
				ok = true,
				offset = offset,
				length = length,
				data = chunk,
			}
		end)
	end

	function api.readBinaryExportBatch(params: { [string]: any }): { [string]: any }
		local exportId, _, session = binaryExportSession(params, true)
		return runSessionOperation(binaryExports, exportId, session, function()
			local denseServices, serviceCount = denseArrayLength(params.services)
			if not denseServices or serviceCount < 2 or serviceCount > #session.serviceNames then
				error("Invalid native export batch services")
			end
			local serviceNames = table.create(serviceCount)
			local included = {}
			for i, rawServiceName in ipairs(params.services) do
				local serviceName = tostring(rawServiceName)
				if serviceName == "" or included[serviceName] or session.nativeStates[serviceName] == nil then
					error("Invalid native export batch service")
				end
				included[serviceName] = true
				serviceNames[i] = serviceName
			end
			local cacheKey = table.concat(serviceNames, PATH_SEPARATOR)
			local batch = session.binaryBatchPayloads[cacheKey]
			if batch == nil then
				if session.structureChanged then
					error("Studio structure changed during native export")
				end
				local timeoutSeconds = math.clamp(tonumber(params.timeoutSeconds) or 30, 1, 120)
				local deadline = os.clock() + timeoutSeconds
				local timeoutThread = task.delay(timeoutSeconds, function()
					if binaryExports[exportId] == session then
						session.payloadReadyEvent:Fire()
					end
				end)
				while os.clock() < deadline do
					if session.cancelled then
						break
					end
					local pending = false
					for _, serviceName in ipairs(serviceNames) do
						if session.payloads[serviceName] == nil and session.payloadErrors[serviceName] == nil then
							pending = true
							break
						end
					end
					if not pending then
						break
					end
					session.payloadReadyEvent.Event:Wait()
				end
				if coroutine.status(timeoutThread) ~= "dead" then
					task.cancel(timeoutThread)
				end
				if session.cancelled then
					error("Native export was cancelled")
				end
				if session.structureChanged then
					error("Studio structure changed during native export")
				end
				batch = session.binaryBatchPayloads[cacheKey]
				if batch == nil then
					local lengths = table.create(serviceCount)
					local batchedServices = table.create(serviceCount)
					local payloadBytes = 0
					local batchCount = 0
					for _, serviceName in ipairs(serviceNames) do
						local payloadError = session.payloadErrors[serviceName]
						if payloadError then
							error(payloadError, 0)
						end
						local payload = session.payloads[serviceName]
						if payload == nil then
							error("Native export serialization timed out")
						end
						local payloadLength = buffer.len(payload)
						local nextCount = batchCount + 1
						local nextTotal = 4 + nextCount * 4 + payloadBytes + payloadLength
						if batchCount > 0 and nextTotal > 67108864 then
							break
						end
						batchCount = nextCount
						payloadBytes += payloadLength
						lengths[batchCount] = payloadLength
						batchedServices[batchCount] = serviceName
					end
					local header = buffer.create(4 + batchCount * 4)
					buffer.writeu32(header, 0, batchCount)
					for i, payloadLength in ipairs(lengths) do
						local entryOffset = 4 + (i - 1) * 4
						buffer.writeu32(header, entryOffset, payloadLength)
					end
					batch = {
						header = header,
						lengths = lengths,
						services = batchedServices,
						totalBytes = buffer.len(header) + payloadBytes,
					}
					session.binaryBatchPayloads[cacheKey] = batch
				end
			end
			local totalBytes = batch.totalBytes
			local offset, length = binaryReadRange(params, totalBytes, "Native export batch")
			local chunk = buffer.create(length)
			local chunkOffset = 0
			local logicalOffset = offset
			local remaining = length
			local headerLength = buffer.len(batch.header)
			if logicalOffset < headerLength then
				local copyLength = math.min(remaining, headerLength - logicalOffset)
				buffer.copy(chunk, chunkOffset, batch.header, logicalOffset, copyLength)
				chunkOffset += copyLength
				logicalOffset += copyLength
				remaining -= copyLength
			end
			if remaining > 0 then
				local payloadOffset = logicalOffset - headerLength
				for i, serviceName in ipairs(batch.services) do
					local payloadLength = batch.lengths[i]
					if payloadOffset >= payloadLength then
						payloadOffset -= payloadLength
					else
						local copyLength = math.min(remaining, payloadLength - payloadOffset)
						buffer.copy(chunk, chunkOffset, session.payloads[serviceName], payloadOffset, copyLength)
						chunkOffset += copyLength
						remaining -= copyLength
						payloadOffset = 0
						if remaining == 0 then
							break
						end
					end
				end
			end
			if remaining ~= 0 then
				error("Native export batch payload is incomplete")
			end
			session.updatedAt = os.clock()
			return encodeBinaryChunk(chunk, offset, length, totalBytes, session.status == "ready")
		end)
	end

	function api.finishBinaryExport(params: { [string]: any }): { [string]: any }
		local exportId = tostring(params.exportId or "")
		local session = binaryExports[exportId]
		local found = session ~= nil
		if type(session) == "table" then
			session.cancelled = true
			session.payloadReadyEvent:Fire()
			expireSession(binaryExports, exportId, session)
		end
		return { ok = true, found = found }
	end

	function api.beginBinaryImport(params: { [string]: any }): { [string]: any }
		pruneExpiredSessions(binaryImports)
		pruneCompletedBinaryImports()
		local importId = tostring(params.importId or "")
		local transactionId = tostring(params.transactionId or "")
		local transaction = editorTransactions[transactionId]
		if type(transaction) ~= "table" then
			error("Native import requires an active editor transaction")
		end
		armSessionExpiry(editorTransactions, transactionId, transaction)
		local totalBytes = tonumber(params.totalBytes)
		local totalChunks = tonumber(params.totalChunks)
		if importId == "" or not totalBytes or totalBytes < 1 or totalBytes > 536870912 or totalBytes % 1 ~= 0 then
			error("Invalid native import size")
		end
		local expectedChunks = math.ceil(totalBytes / BINARY_IMPORT_CHUNK_BYTES)
		if
			not totalChunks
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
		if type(params.externalReferencesPostApplied) ~= "boolean" then
			error("Native import external reference policy is invalid")
		end
		local instanceCount = tonumber(params.instanceCount)
		if not instanceCount or instanceCount ~= instanceCount or instanceCount < 0 or instanceCount % 1 ~= 0 then
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
		local payloadRootNames = {}
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
			if not count or count < 0 or count % 1 ~= 0 then
				error("Invalid native import service count")
			end
			local rootPathsAreArray, rootPathCount = denseArrayLength(rawGroup.rootPaths)
			if not rootPathsAreArray or rootPathCount ~= count then
				error("Invalid native import root paths")
			end
			local rootPaths = table.create(count)
			local rootPathKeys = {}
			for rootIndex, descriptor in ipairs(rawGroup.rootPaths) do
				validateObjectTable(descriptor, "Native import root path")
				validateMutationPath(descriptor, serviceName, `Native import root path {rootIndex}`)
				if
					#descriptor.pathSegments ~= targetPathLength + 1
					or #descriptor.pathOrdinals ~= #descriptor.pathSegments
				then
					error("Native import root path has an invalid length")
				end
				for index = 1, targetPathLength do
					if descriptor.pathSegments[index] ~= targetPath[index] then
						error("Native import root path is outside its target")
					end
				end
				local key = pathCacheKey(descriptor.pathSegments, descriptor.pathOrdinals)
				if rootPathKeys[key] then
					error("Duplicate native import root path")
				end
				rootPathKeys[key] = true
				rootPaths[rootIndex] = {
					pathSegments = table.clone(descriptor.pathSegments),
					pathOrdinals = table.clone(descriptor.pathOrdinals),
				}
			end
			local payloadRootName = rawGroup.payloadRootName
			if type(payloadRootName) ~= "string" or payloadRootName == "" then
				error("Invalid native import payload root")
			end
			if payloadRootNames[payloadRootName] then
				error("Duplicate native import payload root")
			end
			payloadRootNames[payloadRootName] = true
			local retainedRoots = {}
			local retainedPayloadIndexes = {}
			if rawGroup.retainedRoots ~= nil then
				local retainedAreArray, retainedCount = denseArrayLength(rawGroup.retainedRoots)
				if not retainedAreArray or retainedCount > count then
					error("Invalid native import retained roots")
				end
				for retainedIndex, descriptor in ipairs(rawGroup.retainedRoots) do
					validateObjectTable(descriptor, "Native import retained root")
					validateMutationPath(descriptor, serviceName, `Native import retained root {retainedIndex}`)
					local pathLength = #descriptor.pathSegments
					if #descriptor.pathOrdinals ~= pathLength or pathLength ~= targetPathLength + 1 then
						error("Native import retained root has an invalid path")
					end
					for index = 1, targetPathLength do
						if descriptor.pathSegments[index] ~= targetPath[index] then
							error("Native import retained root is outside its target")
						end
					end
					local className = descriptor.className
					local payloadIndex = tonumber(descriptor.payloadIndex)
					local retainedInstanceCount = tonumber(descriptor.instanceCount)
					if
						type(className) ~= "string"
						or className == ""
						or not payloadIndex
						or payloadIndex < 1
						or payloadIndex > count
						or payloadIndex % 1 ~= 0
						or retainedPayloadIndexes[payloadIndex]
						or not retainedInstanceCount
						or retainedInstanceCount < 1
						or retainedInstanceCount % 1 ~= 0
					then
						error("Native import retained root is invalid")
					end
					retainedPayloadIndexes[payloadIndex] = true
					retainedRoots[#retainedRoots + 1] = {
						pathSegments = table.clone(descriptor.pathSegments),
						pathOrdinals = table.clone(descriptor.pathOrdinals),
						className = className,
						payloadIndex = payloadIndex,
						instanceCount = retainedInstanceCount,
					}
				end
			end
			local packageRoots = validatedPackageDescriptors(
				rawGroup.packageRoots,
				"native import package root",
				serviceName,
				targetPath,
				count
			)
			local stripPackagePayloads = {}
			local stripPackagePayloadIndexes = {}
			if rawGroup.stripPackagePayloads ~= nil then
				local stripsAreArray, stripCount = denseArrayLength(rawGroup.stripPackagePayloads)
				if not stripsAreArray or stripCount > count then
					error("Invalid native import package payloads")
				end
				for _, rawIndex in ipairs(rawGroup.stripPackagePayloads) do
					local payloadIndex = tonumber(rawIndex)
					if
						not payloadIndex
						or payloadIndex < 1
						or payloadIndex > count
						or payloadIndex % 1 ~= 0
						or retainedPayloadIndexes[payloadIndex]
						or stripPackagePayloadIndexes[payloadIndex]
					then
						error("Invalid native import package payload")
					end
					stripPackagePayloadIndexes[payloadIndex] = true
					stripPackagePayloads[#stripPackagePayloads + 1] = payloadIndex
				end
			end
			local changeGeneration = tonumber(rawGroup.changeGeneration)
			if
				(#retainedRoots > 0 or #packageRoots > 0)
				and (not changeGeneration or changeGeneration < 0 or changeGeneration % 1 ~= 0)
			then
				error("Native import retained roots require a Studio change generation")
			end
			groups[#groups + 1] = {
				serviceName = serviceName,
				service = service,
				target = target,
				targetPath = table.clone(targetPath),
				count = count,
				payloadRootName = payloadRootName,
				rootPaths = rootPaths,
				retainedRoots = retainedRoots,
				packageRoots = packageRoots,
				stripPackagePayloads = stripPackagePayloads,
				changeGeneration = changeGeneration,
			}
		end
		binaryImports[importId] = {
			transactionId = transactionId,
			totalBytes = totalBytes,
			totalChunks = totalChunks,
			payload = buffer.create(totalBytes),
			received = table.create(totalChunks),
			receivedBytes = 0,
			receivedChunks = 0,
			instanceCount = instanceCount,
			groups = groups,
			externalReferencesPostApplied = params.externalReferencesPostApplied,
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
		local transaction = editorTransactions[session.transactionId]
		if type(transaction) ~= "table" then
			error("Native import editor transaction was not found")
		end
		armSessionExpiry(editorTransactions, session.transactionId, transaction)
		armSessionExpiry(binaryImports, importId, session)
		local index = tonumber(params.index)
		if not index or index < 1 or index > session.totalChunks or index % 1 ~= 0 then
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
		local transactionId = tostring(session.transactionId or "")
		local transaction = editorTransactions[transactionId]
		if type(transaction) ~= "table" then
			error("Native import editor transaction was not found")
		end
		local operationGeneration = cancellationGeneration
		assertSessionOwnership(operationGeneration)
		armSessionExpiry(binaryImports, importId, session)
		armSessionExpiry(editorTransactions, transactionId, transaction)
		beginSessionOperation(session)
		beginSessionOperation(transaction)
		local roots = {}
		local detachedRoots = {}
		local okFinish, responseOrError = xpcall(function()
			local function assertImportActive()
				assertSessionOwnership(operationGeneration)
				if session.cancelRequested then
					error("Native import was cancelled")
				end
				if session.expireRequested or transaction.expireRequested then
					error("Native import session expired")
				end
			end
			local started = os.clock()
			local previousCamera = Workspace.CurrentCamera
			local outgoingByGroup = {}
			local generationsByService = {}
			for groupIndex, group in ipairs(session.groups) do
				if
					(#group.retainedRoots > 0 or #group.packageRoots > 0)
					and ctx.studioChangeGeneration(group.serviceName) ~= group.changeGeneration
				then
					error(`Studio changed {group.serviceName} after package preflight; retry the sync`)
				end
				local outgoing = {}
				for _, instance in ipairs(group.target:GetChildren()) do
					local lockedStarterContainer = group.serviceName == "StarterPlayer"
						and group.target == group.service
						and (instance:IsA("StarterPlayerScripts") or instance:IsA("StarterCharacterScripts"))
					local protectedCamera = group.target == Workspace
						and (instance == previousCamera or isProtectedWorkspaceCameraInstance(instance))
					if
						not instance:IsA("Terrain")
						and not lockedStarterContainer
						and not protectedCamera
						and includeManagedInstance(ctx, group.serviceName, instance)
					then
						outgoing[#outgoing + 1] = instance
					end
				end
				outgoingByGroup[groupIndex] = outgoing
				group.packageScanRoots = outgoing
				ReferenceOverlay.assertPackageRoots(group)
				if generationsByService[group.serviceName] == nil then
					generationsByService[group.serviceName] = ctx.studioChangeGeneration(group.serviceName)
				end
			end
			transaction.nativeGuard = ReferenceOverlay.beginNativeGuard(session.groups)
			roots = SerializationService:DeserializeInstancesAsync(session.payload)
			assertImportActive()
			ReferenceOverlay.assertNativeGuard(transaction.nativeGuard)
			for serviceName, generation in pairs(generationsByService) do
				if ctx.studioChangeGeneration(serviceName) ~= generation then
					error(`Studio changed {serviceName} while native import was preparing; retry the sync`)
				end
			end
			if #roots ~= #session.groups then
				error("Native import returned an unexpected root count")
			end
			local wrappedRootsByName = {}
			for _, root in ipairs(roots) do
				if root.Parent ~= nil or not root:IsA("Folder") or wrappedRootsByName[root.Name] ~= nil then
					error("Native import returned an invalid payload root")
				end
				wrappedRootsByName[root.Name] = root
			end
			local prepared = {}
			local skippedIncomingInstanceCount = 0
			for groupIndex, group in ipairs(session.groups) do
				local groupPayloadRoot = wrappedRootsByName[group.payloadRootName]
				if groupPayloadRoot == nil then
					error("Native import payload group was not found")
				end
				local groupRoots = groupPayloadRoot:GetChildren()
				if #groupRoots ~= group.count then
					error("Native import payload group has the wrong root count")
				end
				local incoming = table.create(group.count)
				local incomingByPayloadIndex = table.create(group.count)
				for index = 1, group.count do
					local instance = groupRoots[index]
					if instance == nil or instance.Parent ~= groupPayloadRoot or instance:IsA("Terrain") then
						error("Native import returned an invalid root")
					end
					incomingByPayloadIndex[index] = instance
					instance.Parent = nil
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
						incomingByPayloadIndex[index] = nil
					else
						incoming[#incoming + 1] = instance
						detachedRoots[#detachedRoots + 1] = instance
					end
				end
				for _, payloadIndex in ipairs(group.stripPackagePayloads) do
					local root = incomingByPayloadIndex[payloadIndex]
					if root == nil or root.Parent ~= nil then
						error("Native import package payload was not detached")
					end
					skippedIncomingInstanceCount += ReferenceOverlay.stripIncomingPackages(root)
				end
				groupPayloadRoot:Destroy()
				prepared[#prepared + 1] = {
					serviceName = group.serviceName,
					service = group.service,
					target = group.target,
					targetPath = group.targetPath,
					rootPaths = group.rootPaths,
					incoming = incoming,
					incomingByPayloadIndex = incomingByPayloadIndex,
					outgoing = outgoingByGroup[groupIndex],
					retainedRoots = group.retainedRoots,
					packageRoots = group.packageRoots,
				}
			end
			local retention = ReferenceOverlay.prepareRetained(prepared, ctx, session.externalReferencesPostApplied)
			transaction.nativeUndo = {
				prepared = prepared,
				replacements = retention.replacements,
				currentCamera = previousCamera,
				resolveStagedPath = retention.resolveStagedPath,
				referenceUpdates = retention.referenceUpdates,
				retainedDuplicates = retention.retainedDuplicates,
				retainedLiveRoots = retention.retainedLiveRoots,
				outgoingScanRoots = retention.outgoingScanRoots,
				generationsByService = generationsByService,
				guard = transaction.nativeGuard,
			}
			transaction.nativeGuard = nil
			transaction.resolveStagedPath = retention.resolveStagedPath
			assertImportActive()
			ReferenceOverlay.apply(retention.referenceOverlay, retention.replacements, nil)
			assertImportActive()
			local elapsed = (os.clock() - started) * 1000
			local createdInstanceCount = math.max(
				0,
				session.instanceCount - skippedIncomingInstanceCount - retention.retainedDuplicateInstanceCount
			)
			transaction.nativeStats = {
				requests = 1,
				lastMs = elapsed,
				instanceCreated = createdInstanceCount,
			}
			local response = {
				ok = true,
				requests = 1,
				instanceCreated = createdInstanceCount,
				rootDeleted = retention.removedRootCount,
				propertyUpdated = retention.referenceUpdates,
				binaryBytes = session.totalBytes,
				binaryMs = elapsed,
				undoRecorded = transaction.historyRecording ~= nil,
			}
			binaryImports[importId] = nil
			completedBinaryImports[importId] = {
				response = response,
				completedAt = os.clock(),
				expiresAt = os.clock() + COMPLETED_BINARY_IMPORT_TTL_SECONDS,
			}
			pruneCompletedBinaryImports()
			return response
		end, function(message)
			return debug.traceback(tostring(message), 2)
		end)
		endSessionOperation(binaryImports, importId, session)
		endSessionOperation(editorTransactions, transactionId, transaction)
		if not okFinish then
			if transaction.nativeUndo ~= nil then
				local undo = transaction.nativeUndo
				transaction.nativeUndo = nil
				pcall(ReferenceOverlay.rollbackNative, undo, ctx)
			end
			ReferenceOverlay.finishNativeGuard(transaction.nativeGuard)
			transaction.nativeGuard = nil
			for _, root in ipairs(roots) do
				if root.Parent == nil then
					root:Destroy()
				end
			end
			for _, root in ipairs(detachedRoots) do
				if root.Parent == nil then
					root:Destroy()
				end
			end
			error(responseOrError, 0)
		end
		return responseOrError
	end

	function api.cancelBinaryImport(params: { [string]: any }): { [string]: any }
		local importId = tostring(params.importId or "")
		local session = binaryImports[importId]
		local found = session ~= nil
		if type(session) == "table" then
			session.cancelRequested = true
			expireSession(binaryImports, importId, session)
		end
		return { ok = true, found = found }
	end

	function api.getFilterCandidates(params: { [string]: any }): { [string]: any }
		if type(params.service) ~= "string" or not ctx.allowedServices[params.service] then
			error("Invalid editor filter service")
		end
		local startIndex = tonumber(params.startIndex) or 1
		local maxCount = tonumber(params.maxCount) or 500
		if startIndex < 1 or startIndex % 1 ~= 0 or maxCount < 1 or maxCount > 500 or maxCount % 1 ~= 0 then
			error("Invalid editor filter page")
		end
		pruneFilterCandidateSnapshots()
		local snapshot = if type(params.snapshotId) == "string"
			then filterCandidateSnapshots[params.snapshotId]
			else nil
		if startIndex == 1 then
			local settingsIdsByInstance = {}
			if params.includeSettingsIds == true then
				for settingsId, instance in pairs(settingsIdLookupForService(params.service, ctx)) do
					local existing = settingsIdsByInstance[instance]
					if existing == nil or (strongSettingsId(settingsId) and not strongSettingsId(existing)) then
						settingsIdsByInstance[instance] = settingsId
					end
				end
			end
			local service = game:GetService(params.service)
			local snapshotItems = {}
			for _, instance in ipairs(service:GetDescendants()) do
				if includeManagedInstance(ctx, params.service, instance) then
					local pathSegments, pathOrdinals = BridgeIdentity.getRefPathParts(instance)
					if pathSegments ~= nil then
						local attributes = {}
						for name in pairs(instance:GetAttributes()) do
							attributes[#attributes + 1] = name
						end
						snapshotItems[#snapshotItems + 1] = {
							pathSegments = pathSegments,
							pathOrdinals = pathOrdinals,
							name = instance.Name,
							className = instance.ClassName,
							settingsId = settingsIdsByInstance[instance],
							tags = CollectionService:GetTags(instance),
							attributes = attributes,
						}
					end
				end
			end
			filterCandidateSnapshotCounter += 1
			local now = os.clock()
			snapshot = {
				id = tostring(filterCandidateSnapshotCounter),
				service = params.service,
				items = snapshotItems,
				createdAt = now,
				expiresAt = now + filterCandidateSnapshotTtlSeconds,
			}
			filterCandidateSnapshots[snapshot.id] = snapshot
			pruneFilterCandidateSnapshots()
		elseif
			type(snapshot) ~= "table"
			or type(params.snapshotId) ~= "string"
			or params.snapshotId ~= snapshot.id
			or snapshot.service ~= params.service
		then
			error("Editor filter snapshot expired")
		end
		snapshot.expiresAt = os.clock() + filterCandidateSnapshotTtlSeconds
		local items = {}
		local lastIndex = math.min(#snapshot.items, startIndex + maxCount - 1)
		for index = startIndex, lastIndex do
			items[#items + 1] = snapshot.items[index]
		end
		local nextIndex = if lastIndex < #snapshot.items then lastIndex + 1 else nil
		if nextIndex == nil then
			filterCandidateSnapshots[snapshot.id] = nil
		end
		return {
			items = items,
			nextIndex = nextIndex,
			snapshotId = snapshot.id,
		}
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
		if type(session) == "table" then
			session.cancelRequested = true
			session.updatedAt = os.clock()
		end
		return { ok = true, found = found }
	end

	function api.applyChanges(params: { [string]: any }): { [string]: any }
		local operationGeneration = cancellationGeneration
		assertSessionOwnership(operationGeneration)
		local serviceNames = validateMutationRequest(params, ctx)
		local requestedTransactionId = tostring(params.transactionId or "")
		local outerTransaction = nil
		if requestedTransactionId ~= "" then
			pruneExpiredSessions(editorTransactions)
			outerTransaction = editorTransactions[requestedTransactionId]
			if type(outerTransaction) ~= "table" then
				error("Editor transaction was not found")
			end
			armSessionExpiry(editorTransactions, requestedTransactionId, outerTransaction)
		end
		local chunkChange = nil
		for _, change in ipairs(params.instanceChanges or {}) do
			local mode = change.mode
			if
				mode == "beginReconcileService"
				or mode == "reconcileServiceChunk"
				or mode == "finishReconcileService"
			then
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
				if outerTransaction == nil then
					transactionSnapshot = captureMutationSnapshot(serviceNames, params, ctx)
				end
			else
				local session = reconcileSessions[chunkSessionKey]
				if type(session) ~= "table" or session.transactionId ~= requestedTransactionId then
					error("Editor reconcile session was not found")
				end
				if outerTransaction == nil then
					if session.rollbackSnapshot == nil then
						error("Editor reconcile session cannot be rolled back")
					end
					transactionSnapshot = session.rollbackSnapshot
				end
			end
		elseif #serviceNames > 0 and outerTransaction == nil then
			transactionSnapshot = captureMutationSnapshot(serviceNames, params, ctx)
		end
		local function assertReconcileActive()
			if chunkSessionKey == nil then
				return
			end
			local session = reconcileSessions[chunkSessionKey]
			if type(session) == "table" and session.cancelRequested then
				error("Editor reconcile was cancelled")
			end
		end
		local started = os.clock()
		local previousResolveCache = ctx.resolveCache
		local previousSettingsIdLookupByService = ctx.settingsIdLookupByService
		local previousMatchCandidateBuckets = ctx.matchCandidateBuckets
		local previousSelectionReplacements = ctx.selectionReplacements
		local previousLoadedMeshPartSources = ctx.loadedMeshPartSources
		local previousReadyMeshPartSources = ctx.readyMeshPartSources
		local previousMeshPartApplyCount = ctx.meshPartApplyCount
		local previousUnreadablePropertyNames = ctx.unreadablePropertyNames
		local previousResolveStagedPath = ctx.resolveStagedPath
		local explorerSelection = captureExplorerSelection()
		local selectionReplacements = {}
		ctx.resolveCache = {}
		ctx.settingsIdLookupByService = {}
		ctx.matchCandidateBuckets = {}
		ctx.selectionReplacements = selectionReplacements
		ctx.loadedMeshPartSources = nil
		ctx.readyMeshPartSources = nil
		ctx.meshPartApplyCount = 0
		ctx.resolveStagedPath = if outerTransaction ~= nil then outerTransaction.resolveStagedPath else nil
		local activeSnapshot = if outerTransaction ~= nil then outerTransaction.snapshot else transactionSnapshot
		ctx.unreadablePropertyNames = if activeSnapshot ~= nil then activeSnapshot.unreadablePropertyNames else nil
		local historyRecording = if outerTransaction ~= nil
			then outerTransaction.historyRecording
			else beginHistoryRecording("Sync from filesystem")
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
			meshPartPreloadCount = 0,
			meshPartPreloadErrors = 0,
			meshPartPreloadMs = 0,
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
				local ok, err = pcall(
					runWithSessionOwnership,
					operationGeneration,
					assertReconcileActive,
					applyInstanceChange,
					change,
					ctx,
					stats,
					touchedServices
				)
				if not ok then
					if type(change) == "table" then
						local mode = tostring(change.mode or "")
						if
							mode == "beginReconcileService"
							or mode == "reconcileServiceChunk"
							or mode == "finishReconcileService"
						then
							local sessionKey =
								reconcileSessionKey(tostring(change.service or ""), change.reconcileSession)
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
				local ok, err = pcall(
					runWithSessionOwnership,
					operationGeneration,
					assertReconcileActive,
					applySourceChange,
					change,
					ctx,
					stats,
					touchedServices
				)
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
			local ok, err = pcall(
				runWithSessionOwnership,
				operationGeneration,
				assertReconcileActive,
				retargetReplacementReferences,
				selectionReplacements,
				ctx,
				stats
			)
			if not ok then
				stats.ok = false
				stats.errors += 1
				warn("[Renium] editor reference sync failed: " .. tostring(err))
				aborted = true
			end
		end
		if not aborted and outerTransaction ~= nil then
			for original, replacement in pairs(selectionReplacements) do
				outerTransaction.instanceReplacements[original] = replacement
			end
		end

		local propertyChanges = params.propertyChanges
		if not aborted and type(propertyChanges) == "table" then
			local okPreload, preloadCountOrError, preloadMs = pcall(
				runWithSessionOwnership,
				operationGeneration,
				assertReconcileActive,
				preloadPropertyMeshPartSources,
				propertyChanges,
				ctx
			)
			if okPreload then
				stats.meshPartPreloadCount = preloadCountOrError
				stats.meshPartPreloadMs = preloadMs
			else
				ctx.readyMeshPartSources = nil
				local ownsSession = pcall(assertSessionOwnership, operationGeneration)
				if ownsSession then
					stats.meshPartPreloadErrors += 1
					warn("[Renium] editor mesh preload failed: " .. tostring(preloadCountOrError))
				else
					stats.ok = false
					stats.errors += 1
					aborted = true
				end
			end
			if not aborted then
				local sliceStarted = os.clock()
				for _, change in ipairs(propertyChanges) do
					local ok, err = pcall(
						runWithSessionOwnership,
						operationGeneration,
						assertReconcileActive,
						applyPropertyChange,
						change,
						ctx,
						stats,
						touchedServices
					)
					if not ok then
						stats.ok = false
						stats.errors += 1
						warn("[Renium] editor property sync failed: " .. tostring(err))
						aborted = true
						break
					end
					if os.clock() - sliceStarted >= 0.008 then
						task.wait()
						sliceStarted = os.clock()
					end
				end
			end
		end

		if not aborted and chunkChange ~= nil and chunkChange.mode == "beginReconcileService" then
			local okSession = pcall(function()
				assertSessionOwnership(operationGeneration)
				assertReconcileActive()
				local session = reconcileSessions[chunkSessionKey]
				if type(session) ~= "table" then
					error("Editor reconcile session was not found")
				end
				session.transactionId = requestedTransactionId
				if outerTransaction == nil then
					session.rollbackSnapshot = transactionSnapshot
					session.onExpire = function()
						rollbackReconcileSnapshot(transactionSnapshot, chunkChange.service)
					end
				end
				assertReconcileActive()
				assertSessionOwnership(operationGeneration)
			end)
			if not okSession then
				stats.ok = false
				stats.errors += 1
				aborted = true
			end
		end

		if stopEventProbe ~= nil then
			task.wait()
			stopEventProbe()
			if
				not aborted
				and not pcall(function()
					assertSessionOwnership(operationGeneration)
					assertReconcileActive()
				end)
			then
				stats.ok = false
				stats.errors += 1
				aborted = true
			end
		end
		local restoredSelectionReplacements = selectionReplacements
		if aborted and transactionSnapshot ~= nil then
			if chunkSessionKey ~= nil then
				reconcileSessions[chunkSessionKey] = nil
			end
			local okRollback, replacements =
				pcall(restoreMutationSnapshot, transactionSnapshot, ctx, selectionReplacements)
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
		if outerTransaction == nil then
			finishHistoryRecording(
				historyRecording,
				if aborted then Enum.FinishRecordingOperation.Cancel else Enum.FinishRecordingOperation.Commit
			)
		end
		restoreExplorerSelection(explorerSelection, restoredSelectionReplacements)
		stats.lastMs = (os.clock() - started) * 1000
		for serviceName in pairs(touchedServices) do
			ctx.invalidateService(serviceName)
		end
		ctx.resolveCache = previousResolveCache
		ctx.settingsIdLookupByService = previousSettingsIdLookupByService
		ctx.matchCandidateBuckets = previousMatchCandidateBuckets
		ctx.selectionReplacements = previousSelectionReplacements
		ctx.loadedMeshPartSources = previousLoadedMeshPartSources
		ctx.readyMeshPartSources = previousReadyMeshPartSources
		ctx.meshPartApplyCount = previousMeshPartApplyCount
		ctx.unreadablePropertyNames = previousUnreadablePropertyNames
		ctx.resolveStagedPath = previousResolveStagedPath
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

	function api.requestCancellation()
		cancellationGeneration += 1
	end

	function api.cleanup()
		local activeTransactions = {}
		for _, session in pairs(editorTransactions) do
			if type(session) == "table" and session.snapshot ~= nil then
				session.onExpire = nil
				activeTransactions[#activeTransactions + 1] = session
			end
		end
		table.clear(editorTransactions)
		for _, session in ipairs(activeTransactions) do
			local okRollback, rollbackError = pcall(runWithStudioChangeSuppression, ctx, function()
				return rollbackTransactionSession(session, ctx)
			end)
			if not okRollback then
				warn("[Renium] transaction cleanup failed: " .. tostring(rollbackError))
			end
			finishHistoryRecording(session.historyRecording, Enum.FinishRecordingOperation.Cancel)
			session.historyRecording = nil
		end
		local activeReconciles = {}
		for _, session in pairs(reconcileSessions) do
			if type(session) == "table" and session.rollbackSnapshot ~= nil then
				table.insert(activeReconciles, session)
			end
		end
		table.clear(reconcileSessions)
		for _, session in ipairs(activeReconciles) do
			local okRollback, rollbackError = pcall(runWithStudioChangeSuppression, ctx, function()
				return rollbackReconcileSnapshot(session.rollbackSnapshot, session.serviceName)
			end)
			if not okRollback then
				warn("[Renium] reconcile cleanup failed: " .. tostring(rollbackError))
			end
		end
		table.clear(binaryImports)
		table.clear(completedBinaryImports)
		for exportId, session in pairs(binaryExports) do
			if type(session) == "table" then
				session.cancelled = true
				session.payloadReadyEvent:Fire()
				expireSession(binaryExports, exportId, session)
			else
				binaryExports[exportId] = nil
			end
		end
		if type(ctx.matchedSettingsInstancesByService) == "table" then
			table.clear(ctx.matchedSettingsInstancesByService)
		end
	end

	return api
end

return BridgeEditorSync
