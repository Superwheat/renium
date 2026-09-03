local BridgeEditorSync = {}

local BridgeCandidateMatch = require(script.Parent.BridgeCandidateMatch)
local BridgeConnection = require(script.Parent.BridgeConnection)
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
local nativeSnapshotPools = {}
local nativePayloadProofs = {}
local editorTransactions = {}
local SESSION_TTL_SECONDS = 120
local TRANSACTION_OUTCOME_TTL_SECONDS = 300
local MAX_TRANSACTION_OUTCOMES = 64
local REQUEST_LEASE_CANCELLATION_TTL_SECONDS = 120
local MAX_RECONCILE_SESSIONS = 16
local MAX_RECONCILE_ENTRIES = 1000000
local MAX_BINARY_IMPORT_SESSIONS = 4
local MAX_BINARY_IMPORT_BUFFERED_BYTES = 536870912
local BINARY_IMPORT_CHUNK_BYTES = 2097152
local COMPLETED_BINARY_IMPORT_TTL_SECONDS = 300
local MAX_COMPLETED_BINARY_IMPORTS = 64
local NATIVE_SERIALIZATION_SERVICE_LIMIT = 4096
local NATIVE_SERIALIZATION_BATCH_LIMIT = 8192
local NATIVE_IDENTITY_CARRIER_CLASS = "HumanoidRigDescription"
local NATIVE_IDENTITY_CARRIER_SLOTS = {
	"Chest",
	"HeadBase",
	"LeftAnkle",
	"LeftClavicle",
	"LeftElbow",
	"LeftHip",
	"LeftKnee",
	"LeftShoulder",
	"LeftToeBase",
	"LeftWrist",
	"Neck",
	"RightAnkle",
	"RightClavicle",
	"RightElbow",
	"RightHip",
	"RightKnee",
	"RightShoulder",
	"RightToeBase",
	"RightWrist",
	"Root",
	"Spine",
	"Waist",
}
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
	serializationComplete: boolean,
	payloadHash: string?,
	knownPayloadHash: string?,
	supportsPayloadCache: boolean?
): { [string]: any }
	if supportsPayloadCache and payloadHash ~= nil and payloadHash == knownPayloadHash then
		return {
			start = 1,
			nextStart = 1,
			total = totalBytes,
			chunk = "",
			pluginEncodeMs = 0,
			serializationComplete = serializationComplete,
			payloadHash = payloadHash,
			payloadCacheHit = true,
		}
	end
	local encodeStarted = os.clock()
	local encoded = buffer.tostring(EncodingService:Base64Encode(chunk))
	return {
		start = offset + 1,
		nextStart = offset + length + 1,
		total = totalBytes,
		chunk = encoded,
		pluginEncodeMs = (os.clock() - encodeStarted) * 1000,
		serializationComplete = serializationComplete,
		payloadHash = if supportsPayloadCache then payloadHash else nil,
		payloadCacheHit = false,
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
		return false
	end
	local onExpire = session.onExpire
	if type(onExpire) == "function" then
		local okExpire, expireError = pcall(onExpire)
		if not okExpire then
			session.rollbackFailed = tostring(expireError)
			session.updatedAt = os.clock()
			warn("[Renium] session expiry cleanup failed: " .. tostring(expireError))
			return false
		end
	end
	session.rollbackFailed = nil
	session.onExpire = nil
	values[key] = nil
	return true
end

local armSessionExpiry

local function beginSessionOperation(session: { [any]: any })
	session.activeOperations = (session.activeOperations or 0) + 1
	session.updatedAt = os.clock()
end

local function endSessionOperation(values: { [any]: any }, key: any, session: { [any]: any })
	session.activeOperations -= 1
	session.updatedAt = os.clock()
	if session.activeOperations == 0 and values[key] == session then
		if session.expireRequested then
			session.expireRequested = nil
			if not expireSession(values, key, session) then
				armSessionExpiry(values, key, session)
			end
		else
			armSessionExpiry(values, key, session)
		end
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

armSessionExpiry = function(values: { [any]: any }, key: any, session: { [any]: any })
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
	local boundary = ChangeHistoryService:TryBeginRecording(
		`Renium:boundary:{os.clock()}`,
		"Renium: Preserve current Studio state"
	)
	if boundary == nil then
		return nil
	end
	local boundaryOk = pcall(
		ChangeHistoryService.FinishRecording,
		ChangeHistoryService,
		boundary,
		Enum.FinishRecordingOperation.Commit
	)
	if not boundaryOk then
		return nil
	end
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

local function markTransactionMutation(ctx: { [string]: any }?)
	if ctx ~= nil and type(ctx.editorTransaction) == "table" then
		ctx.editorTransaction.mutated = true
	end
end

local function markLiveMutation(ctx: { [string]: any }?, instance: Instance?)
	if instance ~= nil and (instance == game or instance:IsDescendantOf(game)) then
		markTransactionMutation(ctx)
	end
end

local function setParentForSync(instance: Instance, parent: Instance?, ctx: { [string]: any }?)
	if instance.Parent == parent then
		return
	end
	if instance:IsA("PackageLink") then
		error("PackageLink instances cannot be reparented")
	end
	local wasLive = instance:IsDescendantOf(game)
	local token = if ctx ~= nil then ctx.expectParentChange(instance, parent) else nil
	local ok, result = pcall(function()
		instance.Parent = parent
	end)
	if not ok then
		cancelExpectedEvent(ctx, token)
		error(result, 0)
	end
	if wasLive or instance:IsDescendantOf(game) then
		markTransactionMutation(ctx)
	end
	if instance.Parent ~= parent then
		cancelExpectedEvent(ctx, token)
		local target = if parent == nil then "nil" else parent:GetFullName()
		error(`Roblox rejected parenting {instance:GetFullName()} to {target}`, 0)
	end
end

local function setNameForSync(instance: Instance, name: string, ctx: { [string]: any }?)
	if instance.Name == name then
		return
	end
	if instance:IsA("PackageLink") then
		error("PackageLink instances cannot be renamed")
	end
	local token = if ctx ~= nil then ctx.expectPropertyEvent(instance, "Name", name) else nil
	local ok, result = pcall(function()
		instance.Name = name
	end)
	if not ok then
		cancelExpectedEvent(ctx, token)
		error(result, 0)
	end
	markLiveMutation(ctx, instance)
	if instance.Name ~= name then
		cancelExpectedEvent(ctx, token)
		error(`Roblox rejected renaming {instance:GetFullName()} to {name}`, 0)
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
	markLiveMutation(ctx, Workspace)
	if Workspace.CurrentCamera ~= camera then
		cancelExpectedEvent(ctx, token)
		error("Roblox did not retain Workspace.CurrentCamera", 0)
	end
end

local function removeInstanceForUndo(instance: Instance, ctx: { [string]: any }?)
	if instance:IsA("PackageLink") then
		error(`PackageLink instances cannot be removed directly: {instance:GetFullName()}`)
	end
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
	if settingsId ~= "1" and not strongSettingsId(settingsId) then
		if type(ctx.matchedSettingsIdByInstance) ~= "table" then
			ctx.matchedSettingsIdByInstance = setmetatable({}, { __mode = "k" })
		end
		if ctx.matchedSettingsIdByInstance[instance] ~= settingsId then
			ctx.matchedSettingsIdByInstance[instance] = settingsId
			ctx.matchedSettingsIdVersion = (tonumber(ctx.matchedSettingsIdVersion) or 0) + 1
		end
	end
	if type(ctx.settingsIdLookupByService) == "table" then
		local cached = ctx.settingsIdLookupByService[serviceName]
		if type(cached) == "table" and type(cached.lookup) == "table" then
			cached.lookup[settingsId] = instance
		end
	end
end

local function clearMatchedSettingsInstances(serviceName: string, ctx: { [string]: any })
	if type(ctx.matchedSettingsInstancesByService) == "table" then
		local serviceMatches = ctx.matchedSettingsInstancesByService[serviceName]
		if type(serviceMatches) == "table" and type(ctx.matchedSettingsIdByInstance) == "table" then
			local service = game:GetService(serviceName)
			for settingsId, candidate in pairs(serviceMatches) do
				local instance = liveInstance(candidate)
				if
					instance ~= nil
					and instance:IsDescendantOf(service)
					and ctx.matchedSettingsIdByInstance[instance] == settingsId
				then
					ctx.matchedSettingsIdByInstance[instance] = nil
					ctx.matchedSettingsIdVersion = (tonumber(ctx.matchedSettingsIdVersion) or 0) + 1
				end
			end
		end
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

local function instanceMatchesExpectedName(instance: Instance, pathSegments: any): boolean
	if type(pathSegments) ~= "table" then
		return true
	end
	local expectedName = pathSegments[#pathSegments]
	return type(expectedName) ~= "string" or instance.Name == expectedName
end

local function matchingChangeInstance(
	instance: Instance?,
	change: { [string]: any },
	allowClassMismatch: boolean?
): Instance?
	if
		instance ~= nil
		and instanceMatchesExpectedName(instance, change.pathSegments)
		and (allowClassMismatch or instanceMatchesExpectedClass(instance, change.className))
	then
		return instance
	end
	return nil
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
		local staged = matchingChangeInstance(
			ctx.resolveStagedPath(pathSegments, change.pathOrdinals),
			change,
			allowClassMismatch
		)
		if staged ~= nil then
			return staged
		end
	end
	local persistent = matchingChangeInstance(
		matchedSettingsInstance(serviceName, change.settingsId, ctx),
		change,
		allowClassMismatch
	)
	if persistent ~= nil then
		return persistent
	end
	if type(pathSegments) == "table" and #pathSegments > 0 then
		local pathInstance = resolvePathSegments(pathSegments, ctx.resolveCache, change.pathOrdinals)
		if
			pathInstance ~= nil and (allowClassMismatch or instanceMatchesExpectedClass(pathInstance, change.className))
		then
			return pathInstance
		end
		return nil
	end
	return matchingChangeInstance(
		resolveInstanceBySettingsId(serviceName, change.settingsId, ctx),
		change,
		allowClassMismatch
	)
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
		if settingsInstance == nil and type(ctx.allowedServices) == "table" then
			for candidateServiceName, allowed in pairs(ctx.allowedServices) do
				if allowed and candidateServiceName ~= targetServiceName then
					local candidate = resolveInstanceBySettingsId(candidateServiceName, settingsId, ctx)
					if candidate ~= nil then
						if settingsInstance ~= nil and settingsInstance ~= candidate then
							return nil
						end
						settingsInstance = candidate
					end
				end
			end
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
	if entry.settingsId ~= nil then
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
	local previousClassName = tostring(entry.previousClassName or "")
	local function classCompatible(candidate: Instance): boolean
		return candidate.ClassName == entry.className
			or (previousClassName ~= "" and candidate.ClassName == previousClassName)
	end
	local pathInstance = resolvePathSegments(entry.pathSegments, nil, entry.pathOrdinals)
	if entry.anchorOnly and not entry.ambiguousSiblings then
		if pathInstance ~= nil and not claimedInstances[pathInstance] and classCompatible(pathInstance) then
			return pathInstance
		end
		return nil
	end
	local persistent = matchedSettingsInstance(serviceName, entry.settingsId, ctx)
	if not entry.anchorOnly and persistent ~= nil and not claimedInstances[persistent] then
		return persistent
	end

	local previousPathInstance = if #entry.previousPathSegments > 0
		then resolvePathSegments(entry.previousPathSegments, nil, entry.previousPathOrdinals)
		else nil
	if
		previousPathInstance ~= nil
		and not claimedInstances[previousPathInstance]
		and classCompatible(previousPathInstance)
	then
		return previousPathInstance
	end
	if
		entry.exactPathIdentity
		and pathInstance ~= nil
		and not claimedInstances[pathInstance]
		and classCompatible(pathInstance)
	then
		return pathInstance
	end
	if not entry.ambiguousSiblings then
		if pathInstance ~= nil and not claimedInstances[pathInstance] and classCompatible(pathInstance) then
			return pathInstance
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
		if not classCompatible(candidate) then
			return
		end
		included[candidate] = true
		candidates[#candidates + 1] = candidate
	end
	include(pathInstance)
	if parent ~= nil then
		local byClass = candidateBucketsForParent(parent, ctx)[expectedName]
		if byClass ~= nil then
			for _, candidateClass in ipairs({ entry.className, previousClassName }) do
				local bucket = byClass[candidateClass]
				if candidateClass ~= "" and type(bucket) == "table" then
					for _, candidate in ipairs(bucket) do
						include(candidate)
					end
				end
			end
		end
	end
	include(persistent)

	local chosen = BridgeCandidateMatch.choose(
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
	if chosen == nil and pathInstance ~= nil and included[pathInstance] then
		-- Identical duplicate siblings have no semantic discriminator. Their exact
		-- pulled ordinal is the only stable local identity and is safe as a tie-break.
		chosen = pathInstance
	end
	if chosen == nil and #candidates > 0 then
		error(`Could not uniquely identify {entry.key}; Studio was not changed`)
	end
	if chosen == nil and pathInstance ~= nil and not classCompatible(pathInstance) then
		error(`Ambiguous class replacement at {entry.key}; reconcile before replacing it`)
	end
	return chosen
end

local function writeProperty(instance: Instance, propertyName: string, value: any): (boolean, any)
	local writableInstance = instance :: any
	if instance:IsA("Model") or instance:IsA("WorldModel") then
		if propertyName == "Scale" then
			return pcall(writableInstance.ScaleTo, instance, value)
		elseif propertyName == "Origin" then
			return pcall(writableInstance.PivotTo, instance, value)
		elseif propertyName == "WorldPivot" or propertyName == "WorldPivotData" then
			return pcall(function()
				writableInstance.WorldPivot = value
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
		writableInstance[propertyName] = value
	end)
	if okWrite then
		return true, nil
	end
	if type(value) == "string" and Content ~= nil and (Content :: any).fromUri ~= nil then
		local okContent = pcall(function()
			writableInstance[propertyName] = if value == ""
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
	if instance:IsA("PackageLink") then
		return false, "PackageLink properties are read-only"
	end
	local token = if ctx ~= nil then ctx.expectPropertyEvent(instance, propertyName, value) else nil
	local ok, result = writeProperty(instance, propertyName, value)
	if not ok then
		cancelExpectedEvent(ctx, token)
		return false, result
	end
	markLiveMutation(ctx, instance)
	local okRead, current = readProperty(instance, propertyName)
	if not okRead or not valuesEqual(current, value) then
		cancelExpectedEvent(ctx, token)
		return false, `Roblox did not retain {propertyName}`
	end
	return true, result
end

local function setAttributeForSync(
	instance: Instance,
	attributeName: string,
	value: any,
	ctx: { [string]: any }?
): (boolean, any)
	if instance:IsA("PackageLink") then
		return false, "PackageLink attributes are read-only"
	end
	local token = if ctx ~= nil then ctx.expectAttributeEvent(instance, attributeName, value) else nil
	local ok, result = pcall(instance.SetAttribute, instance, attributeName, value)
	if not ok then
		cancelExpectedEvent(ctx, token)
		return false, result
	end
	markLiveMutation(ctx, instance)
	if not valuesEqual(instance:GetAttribute(attributeName), value) then
		cancelExpectedEvent(ctx, token)
		return false, `Roblox did not retain attribute {attributeName}`
	end
	return true, result
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
	if instance:IsA("PackageLink") then
		error("PackageLink tags are read-only")
	end
	local token = ctx.expectTagChange(instance, tag, added)
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
	markLiveMutation(ctx, instance)
	if CollectionService:HasTag(instance, tag) ~= added then
		cancelExpectedEvent(ctx, token)
		error(`Roblox did not retain tag {tag} on {instance:GetFullName()}`, 0)
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
	if containsPackageLink(instance) then
		error(`Ordinary sync cannot replace package-bearing instance {instance:GetFullName()}`)
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
	CollectionService = CollectionService,
	RbxDomModule = RbxDomModule,
	captureExplorerSelection = captureExplorerSelection,
	containsPackageLink = containsPackageLink,
	pathCacheKey = pathCacheKey,
	readProperty = readProperty,
	removeInstanceForUndo = removeInstanceForUndo,
	resolveOrdinalChild = resolveOrdinalChild,
	resolvePathSegments = resolvePathSegments,
	restoreExplorerSelection = restoreExplorerSelection,
	setAttributeForSync = setAttributeForSync,
	setCurrentCameraForSync = setCurrentCameraForSync,
	setParentForSync = setParentForSync,
	setTagForSync = setTagForSync,
	valuesEqual = valuesEqual,
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

local function normalizedSource(source: string): string
	if string.find(source, "\r", 1, true) == nil then
		return source
	end
	return (string.gsub(string.gsub(source, "\r\n", "\n"), "\r", "\n"))
end

local function verifySourceWrite(instance: Instance, expectedSource: string)
	local okRead, appliedSource = readScriptSource(instance)
	if not okRead then
		error(`Failed to verify Source for {instance:GetFullName()}: {appliedSource}`)
	end
	if normalizedSource(appliedSource) ~= normalizedSource(expectedSource) then
		error(
			`Source verification failed for {instance:GetFullName()}: expected {#expectedSource} bytes, got {#appliedSource} bytes`
		)
	end
end

local function assertSourceContainer(instance: Instance, ctx: { [string]: any })
	if not ctx.luaSourceClass[instance.ClassName] then
		error("Target is not a Lua source container: " .. instance:GetFullName())
	end
end

local function writeSourceIfChanged(
	instance: Instance,
	nextSource: string,
	action: string,
	ctx: { [string]: any },
	stats: { [string]: any }
): boolean
	local okRead, currentSource = readScriptSource(instance)
	if okRead and normalizedSource(currentSource) == normalizedSource(nextSource) then
		return false
	end

	local okWrite, writeError, writeMethod = setSource(instance, nextSource, ctx)
	if not okWrite then
		error(`Failed to {action} Source for {instance:GetFullName()}: {writeError}`)
	end
	markLiveMutation(ctx, instance)
	verifySourceWrite(instance, nextSource)
	if writeMethod == "UpdateSourceAsync" then
		stats.sourceUpdateAsync += 1
	else
		stats.sourceDirect += 1
	end
	return true
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
		if instance == nil then
			stats.noops += 1
			return
		end
		assertSourceContainer(instance, ctx)
		if writeSourceIfChanged(instance, "", "clear", ctx, stats) then
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
	assertSourceContainer(instance, ctx)

	local nextSource = tostring(change.source or "")
	if writeSourceIfChanged(instance, nextSource, "write", ctx, stats) then
		stats.sourceUpdated += 1
	else
		stats.noops += 1
	end
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
				if
					#pathSegments > 1
					and className ~= serviceName
					and (className ~= "PackageLink" or change.mode == "deleteInstances")
				then
					entries[#entries + 1] = {
						pathSegments = pathSegments,
						pathOrdinals = cloneArray(raw.pathOrdinals),
						previousPathSegments = cloneArray(raw.previousPathSegments),
						previousPathOrdinals = cloneArray(raw.previousPathOrdinals),
						key = pathCacheKey(pathSegments, raw.pathOrdinals),
						className = className,
						previousClassName = if type(raw.previousClassName) == "string" then raw.previousClassName else nil,
						settingsId = settingsIdText(raw.settingsId),
						ambiguousSiblings = raw.ambiguousSiblings == true,
						exactPathIdentity = change.mode == "deleteInstances",
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

	local instance = liveInstance(resolvedEntries[entry.key])
	if instance == nil then
		instance = resolveEntryInstance(entry, serviceName, ctx, resolvedEntries, claimedInstances)
	end
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
		local parent = resolveEntryParent(entry, resolvedEntries)
		if parent == nil then
			error("Cannot place instance; parent path was not found: " .. tostring(entry.key))
		end
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
	local unknown = {}
	local removedCount = 0
	for _, instance in descendants do
		local pathSegments, pathOrdinals = BridgeIdentity.getRefPathParts(instance)
		local key = if pathSegments then pathCacheKey(pathSegments, pathOrdinals) else ""
		if
			key ~= ""
			and not instance:IsA("PackageLink")
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
			unknown[instance] = true
			removedCount += 1
		end
	end
	for instance in unknown do
		if not unknown[instance.Parent] then
			removeInstanceForUndo(instance, ctx)
		end
	end
	stats.instanceDeleted += removedCount
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
	local beforePropertyUpdated = stats.propertyUpdated
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
		and stats.propertyUpdated == beforePropertyUpdated
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
	for _, raw in ipairs(change.instances or {}) do
		if type(raw) == "table" and raw.className == "PackageLink" then
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
	local beforePropertyUpdated = stats.propertyUpdated
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
		local instance = syncDesiredEntry(
			entry,
			service.Name,
			ctx,
			stats,
			session.resolvedEntries,
			session.claimedInstances,
			true
		)
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
		and stats.propertyUpdated == beforePropertyUpdated
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
	local beforePropertyUpdated = stats.propertyUpdated
	local entries = sortedInstanceEntries(change, service.Name)
	local resolvedEntries = {}
	local claimedInstances = {}
	local createMissing = liveHydrateEnabled(ctx)
	for _, entry in ipairs(entries) do
		if #entry.previousPathSegments > 0 then
			local instance = resolveEntryInstance(entry, service.Name, ctx, resolvedEntries, claimedInstances)
			if instance == nil then
				error(`Could not identify the previous instance at {entry.key}; Studio was not changed`)
			end
			resolvedEntries[entry.key] = instance
			rememberEntryResolution(entry, service.Name, instance, claimedInstances, ctx)
		end
	end
	for _, entry in ipairs(entries) do
		syncDesiredEntry(entry, service.Name, ctx, stats, resolvedEntries, claimedInstances, createMissing)
	end

	if
		stats.instanceCreated == beforeCreated
		and stats.instanceReplaced == beforeReplaced
		and stats.propertyUpdated == beforePropertyUpdated
	then
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
	local resolvedEntries = {}
	local claimedInstances = {}
	for _, entry in ipairs(sortedInstanceEntries(change, service.Name)) do
		if #entry.pathSegments <= 1 then
			error("Refusing to delete service root: " .. entry.key)
		end
		local instance = resolveEntryInstance(entry, service.Name, ctx, resolvedEntries, claimedInstances)
		if instance == nil then
			stats.noops += 1
			continue
		end
		if isProtectedWorkspaceCameraInstance(instance) or seenTargets[instance] then
			stats.noops += 1
		elseif instance:IsA("PackageLink") then
			error(`PackageLink instances cannot be removed directly: {instance:GetFullName()}`)
		else
			rememberEntryResolution(entry, service.Name, instance, claimedInstances, ctx)
			seenTargets[instance] = true
			targets[#targets + 1] = instance
		end
	end
	for _, instance in ipairs(targets) do
		removeInstanceForUndo(instance, ctx)
		if instance.Parent ~= nil then
			error(`Studio did not remove {instance:GetFullName()}`)
		end
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

local function errorContains(err, phrases)
	local errText = string.lower(tostring(err))
	for _, phrase in ipairs(phrases) do
		if string.find(errText, phrase, 1, true) then
			return true
		end
	end
	return false
end

local PROTECTED_PROPERTY_WRITE_ERRORS = {
	"read only",
	"lacking capability",
	"not accessible",
	"not a valid member",
}

local PROTECTED_ATTRIBUTE_WRITE_ERRORS = {
	"corescript permission required",
	"read only",
}

local function changedPropertyNames(properties)
	local names = {}
	if properties.MeshId ~= nil then
		names[1] = "MeshId"
	end
	for propertyName in pairs(properties) do
		propertyName = tostring(propertyName)
		if propertyName ~= "MeshId" then
			names[#names + 1] = propertyName
		end
	end
	return names
end

local function writeDecodedProperty(instance, propertyName, decoded, ctx, stats)
	local okRead, current = readProperty(instance, propertyName)
	if okRead and valuesEqual(current, decoded) then
		stats.noops += 1
		return true, nil
	end
	local okWrite, err = writePropertyForSync(instance, propertyName, decoded, ctx)
	if not okWrite and propertyName == "MeshId" and instance:IsA("MeshPart") then
		local okApplyMesh, applyMeshErr = applyMeshPartMeshId(instance, decoded, ctx)
		if not okApplyMesh then
			error(`Failed to apply MeshId on {instance:GetFullName()}: {applyMeshErr}`)
		end
		okWrite = true
	end
	if not okWrite then
		return false, err
	end
	if propertyName == "MeshId" and instance:IsA("MeshPart") then
		local okRetained, retained = readProperty(instance, propertyName)
		if not okRetained or not valuesEqual(retained, decoded) then
			error(`Roblox did not retain {propertyName} on {instance:GetFullName()}`)
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
	return true, nil
end

local function applyChangedProperty(instance, propertyName, rawValue, change, ctx, stats, unreadableNames, serviceName)
	if propertyName == "Source" then
		stats.noops += 1
		return
	end
	if propertyName == "ClassName" then
		if tostring(rawValue) == instance.ClassName then
			stats.noops += 1
			return
		end
		error("ClassName changes are not supported for " .. instance:GetFullName())
	end
	if propertyName == "Name" then
		local nextName = tostring(rawValue)
		if instance.Name == nextName then
			stats.noops += 1
		else
			setNameForSync(instance, nextName, ctx)
			stats.propertyUpdated += 1
		end
		return
	end
	if propertyName == "Tags" then
		applyTags(instance, rawValue, stats, ctx)
		return
	end
	if not classHasProperty(instance, propertyName) then
		error(`{propertyName} is not a property of {instance.ClassName}`)
	end
	if type(unreadableNames) == "table" and unreadableNames[propertyName] then
		recordProtectedWrite(stats, change, "property", propertyName, rawValue)
		stats.noops += 1
		return
	end
	local okDecode, decoded = decodePropertyValue(instance, propertyName, rawValue, ctx, serviceName)
	if not okDecode then
		error(`Failed to decode {propertyName}: {decoded}`)
	end
	local okWrite, err = writeDecodedProperty(instance, propertyName, decoded, ctx, stats)
	if okWrite then
		return
	end
	if errorContains(err, PROTECTED_PROPERTY_WRITE_ERRORS) then
		recordProtectedWrite(stats, change, "property", propertyName, rawValue)
		stats.noops += 1
		return
	end
	error(`Failed to write {propertyName} on {instance:GetFullName()}: {err}`)
end

local function resetProperty(instance, propertyName, ctx, stats, unreadableNames)
	if propertyName == "Tags" then
		applyTags(instance, {}, stats, ctx)
		return
	end
	if propertyName == "Source" or propertyName == "ClassName" or propertyName == "Name" then
		error(`Property {propertyName} cannot be reset`)
	end
	if not classHasProperty(instance, propertyName) then
		error(`{propertyName} is not a property of {instance.ClassName}`)
	end
	if type(unreadableNames) == "table" and unreadableNames[propertyName] then
		error(`Cannot reset unreadable property {propertyName} on {instance:GetFullName()}`)
	end
	local okCreate, defaultInstance = pcall(Instance.new, instance.ClassName)
	if not okCreate or defaultInstance == nil then
		error(`Cannot determine the default value of {propertyName} on {instance.ClassName}`)
	end
	local okDefault, defaultValue = readProperty(defaultInstance, propertyName)
	defaultInstance:Destroy()
	if not okDefault then
		error(`Cannot read the default value of {propertyName} on {instance.ClassName}`)
	end
	local okRead, current = readProperty(instance, propertyName)
	if okRead and valuesEqual(current, defaultValue) then
		stats.noops += 1
		return
	end
	local okWrite, err = writePropertyForSync(instance, propertyName, defaultValue, ctx)
	if not okWrite then
		error(`Failed to reset {propertyName} on {instance:GetFullName()}: {err}`)
	end
	stats.propertyUpdated += 1
end

local function deleteAttribute(instance, attributeName, change, ctx, stats)
	if instance:GetAttribute(attributeName) == nil then
		stats.noops += 1
		return
	end
	local okWrite, err = setAttributeForSync(instance, attributeName, nil, ctx)
	if okWrite then
		stats.attributeUpdated += 1
		return
	end
	if errorContains(err, PROTECTED_ATTRIBUTE_WRITE_ERRORS) then
		recordProtectedWrite(stats, change, "attribute", attributeName, nil, true)
		stats.noops += 1
		return
	end
	error(`Failed to delete attribute {attributeName} on {instance:GetFullName()}: {err}`)
end

local function applyChangedAttribute(instance, attributeName, rawValue, change, ctx, stats)
	local okDecode, decoded = decodeValue(rawValue, nil)
	if not okDecode then
		error(`Failed to decode attribute {attributeName}: {decoded}`)
	end
	if valuesEqual(instance:GetAttribute(attributeName), decoded) then
		stats.noops += 1
		return
	end
	local okWrite, err = setAttributeForSync(instance, attributeName, decoded, ctx)
	if okWrite then
		stats.attributeUpdated += 1
		return
	end
	if errorContains(err, PROTECTED_ATTRIBUTE_WRITE_ERRORS) then
		recordProtectedWrite(stats, change, "attribute", attributeName, rawValue)
		stats.noops += 1
		return
	end
	error(`Failed to write attribute {attributeName} on {instance:GetFullName()}: {err}`)
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
		error("PackageLink instances are read-only")
	end

	local instance = resolveInstance(change, ctx)
	if instance == nil then
		error(
			`Target instance was not found: {table.concat(cloneArray(change.pathSegments), ".")} [{change.className or ""}]`
		)
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
	local unreadableNames = if type(ctx.unreadablePropertyNames) == "table"
		then ctx.unreadablePropertyNames[instance]
		else nil
	local properties = change.properties
	if type(properties) == "table" then
		for _, propertyName in ipairs(changedPropertyNames(properties)) do
			applyChangedProperty(
				instance,
				propertyName,
				properties[propertyName],
				change,
				ctx,
				stats,
				unreadableNames,
				serviceName
			)
		end
	end

	local resetProperties = change.resetProperties
	if type(resetProperties) == "table" then
		for _, propertyName in ipairs(resetProperties) do
			resetProperty(instance, propertyName, ctx, stats, unreadableNames)
		end
	end

	local deletedAttributes = change.deletedAttributes
	if type(deletedAttributes) == "table" then
		for _, attributeName in ipairs(deletedAttributes) do
			deleteAttribute(instance, attributeName, change, ctx, stats)
		end
	end

	local attributes = change.attributes
	if type(attributes) == "table" then
		for attributeName, rawValue in pairs(attributes) do
			attributeName = tostring(attributeName)
			applyChangedAttribute(instance, attributeName, rawValue, change, ctx, stats)
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

local function validateMutationPath(
	change: { [string]: any },
	serviceName: string,
	label: string,
	ctx: { [string]: any }
)
	local pathIsArray, pathLength = denseArrayLength(change.pathSegments)
	if not pathIsArray or pathLength == 0 then
		error(label .. " pathSegments must be a non-empty array")
	end
	if pathLength > (tonumber(ctx.maxPathSegments) or 128) then
		error(label .. " path has too many segments")
	end
	for index, segment in ipairs(change.pathSegments) do
		if type(segment) ~= "string" or segment == "" or #segment > 255 then
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
	count: number,
	ctx: { [string]: any }
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
		validateMutationPath(descriptor, serviceName, `{label} {descriptorIndex}`, ctx)
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

local INSTANCE_CHANGE_MODES = {
	reconcileService = true,
	beginReconcileService = true,
	reconcileServiceChunk = true,
	finishReconcileService = true,
	upsertInstances = true,
	replaceInstances = true,
	deleteInstances = true,
}

local CHUNKED_RECONCILE_MODES = {
	beginReconcileService = true,
	reconcileServiceChunk = true,
	finishReconcileService = true,
}

local function validateInstanceChange(change, serviceName, ctx, classCache)
	local mode = change.mode
	if not INSTANCE_CHANGE_MODES[mode] then
		error("Editor instance change has an unsupported mode")
	end
	if change.allowDeletes ~= nil and type(change.allowDeletes) ~= "boolean" then
		error("Editor instance allowDeletes must be a boolean")
	end
	if CHUNKED_RECONCILE_MODES[mode] and (type(change.reconcileSession) ~= "string" or change.reconcileSession == "") then
		error("Editor chunked reconcile requires a session id")
	end
	local maxEntries = tonumber(ctx.maxInstanceEntriesPerChange) or 5000
	local instancesAreArray, instanceCount = denseArrayLength(change.instances)
	if not instancesAreArray or instanceCount > maxEntries then
		error("Editor instance entries must be a bounded array")
	end
	local preserveInstances = change.preserveInstances or {}
	local preservesAreArray, preserveCount = denseArrayLength(preserveInstances)
	if not preservesAreArray or preserveCount > maxEntries then
		error("Editor preserve entries must be a bounded array")
	end
	for preserveIndex, preserve in ipairs(preserveInstances) do
		if type(preserve) ~= "table" then
			error(string.format("Editor preserve entry %d must be an object", preserveIndex))
		end
		validateMutationPath(
			preserve,
			serviceName,
			string.format("Editor preserve entry %d", preserveIndex),
			ctx
		)
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
			string.format("Editor instance entry %d", entryIndex),
			ctx
		)
		if entry.className == "PackageLink" and mode ~= "reconcileService" then
			error("PackageLink instances cannot be created, replaced, or removed directly")
		end
		if
			entry.anchorOnly ~= true
			and entry.className ~= "PackageLink"
			and not isEngineManagedContainerEntry(serviceName, entry)
		then
			validateCreatableClass(entry.className, classCache, "Editor instance entry")
		end
		if entry.matchProperties ~= nil then
			validateObjectTable(entry.matchProperties, "Editor instance matchProperties")
		end
		if entry.matchAttributes ~= nil then
			validateObjectTable(entry.matchAttributes, "Editor instance matchAttributes")
		end
	end
end

local function validateSourceChange(change, ctx, classCache, requireSourcePayload: boolean)
	local className = validateCreatableClass(change.className, classCache, "Editor source change")
	if not ctx.luaSourceClass[className] then
		error("Editor source class is not a Lua source container")
	end
	if change.deleted ~= nil and type(change.deleted) ~= "boolean" then
		error("Editor source deleted must be a boolean")
	end
	if requireSourcePayload and change.deleted ~= true and type(change.source) ~= "string" then
		error("Editor source must be a string")
	end
	if type(change.source) == "string" and #change.source > (tonumber(ctx.maxSourceBytes) or 8 * 1024 * 1024) then
		error("Editor source mutation exceeds safe size limit")
	end
end

local function validateExclusiveNames(values, changed, label, itemLabel, conflictLabel, conflictAction)
	if values == nil then
		return
	end
	local valuesAreArray = denseArrayLength(values)
	if not valuesAreArray then
		error(`Editor {label} must be an array`)
	end
	local seen = {}
	for index, name in ipairs(values) do
		if type(name) ~= "string" or name == "" then
			error(string.format("Editor %s %d must be a non-empty string", itemLabel, index))
		end
		if seen[name] then
			error(`Editor {label} must not contain duplicates`)
		end
		if type(changed) == "table" and changed[name] ~= nil then
			error(`Editor {conflictLabel} cannot be updated and {conflictAction} in one change`)
		end
		seen[name] = true
	end
end

local function validatePropertyChange(change, serviceName, ctx)
	if change.className == "PackageLink" then
		error("PackageLink instances are read-only")
	end
	if type(change.className) ~= "string" then
		error("Editor property className must be a string")
	end
	if change.properties ~= nil then
		validateObjectTable(change.properties, "Editor properties")
	end
	validateExclusiveNames(
		change.resetProperties,
		change.properties,
		"resetProperties",
		"reset property",
		"property",
		"reset"
	)
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
	validateExclusiveNames(
		change.deletedAttributes,
		change.attributes,
		"deletedAttributes",
		"deleted attribute",
		"attribute",
		"deleted"
	)
end

local function validateChangeList(
	rawChanges,
	kind,
	ctx,
	serviceSet,
	classCache,
	maxChanges,
	requireSourcePayload: boolean
)
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
			validateMutationPath(
				change,
				serviceName,
				string.format("Editor %s change %d", kind, changeIndex),
				ctx
			)
		end
		if kind == "instance" then
			validateInstanceChange(change, serviceName, ctx, classCache)
		elseif kind == "source" then
			validateSourceChange(change, ctx, classCache, requireSourcePayload)
		else
			validatePropertyChange(change, serviceName, ctx)
		end
	end
end

local function validateMutationRequest(
	params: any,
	ctx: { [string]: any },
	requireSourcePayload: boolean?
): { string }
	if type(params) ~= "table" then
		error("Editor mutation request must be an object")
	end
	if params.probeEvents ~= nil and type(params.probeEvents) ~= "boolean" then
		error("Editor mutation probeEvents must be a boolean")
	end
	local serviceSet = {}
	local classCache = {}
	local maxChanges = tonumber(ctx.maxChangesPerRequest) or 5000
	local sourcePayloadRequired = requireSourcePayload ~= false
	validateChangeList(params.instanceChanges, "instance", ctx, serviceSet, classCache, maxChanges, sourcePayloadRequired)
	validateChangeList(params.sourceChanges, "source", ctx, serviceSet, classCache, maxChanges, sourcePayloadRequired)
	validateChangeList(params.propertyChanges, "property", ctx, serviceSet, classCache, maxChanges, sourcePayloadRequired)
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

local function mutationSnapshotLayout(
	serviceNames: { string },
	ctx: { [string]: any },
	packageSnapshotRoots: { [Instance]: boolean }?,
	mutationRootsByService: { [string]: { [Instance]: boolean } }?,
	restrictedMutationServices: { [string]: boolean }?
)
	local groups = {}
	local roots = {}
	local metadataTargets = {}
	local metadataSeen = {}
	for _, serviceName in ipairs(serviceNames) do
		local service = game:GetService(serviceName)
		local preserved = {}
		local mutationRoots = if mutationRootsByService ~= nil then mutationRootsByService[serviceName] else nil
		local restricted = restrictedMutationServices ~= nil and restrictedMutationServices[serviceName] == true
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
					if not restricted or (mutationRoots ~= nil and mutationRoots[container]) then
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
		end
		local children = {}
		for _, child in ipairs(service:GetChildren()) do
			local included = includeManagedInstance(ctx, serviceName, child)
			if
				(restricted and not (mutationRoots and mutationRoots[child]))
				or not included
				or (containsPackageLink(child) and not (packageSnapshotRoots and packageSnapshotRoots[child]))
			then
				preserved[child] = true
			elseif not preserved[child] then
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

local function serializeSnapshotRoots(roots: { Instance }, nonArchivable: { Instance }, ctx: { [string]: any })
	local changed = {}
	if #nonArchivable > 0 then
		ctx.beginStudioChangeSuppression(0)
	end
	local ok, payload = xpcall(function()
		for _, instance in ipairs(nonArchivable) do
			if instance.Parent ~= nil and not instance.Archivable then
				local okWrite, writeError = writePropertyForSync(instance, "Archivable", true, ctx)
				if not okWrite then
					error(writeError, 0)
				end
				changed[#changed + 1] = instance
			end
		end
		return SerializationService:SerializeInstancesAsync(roots)
	end, debug.traceback)
	local restoreError
	for index = #changed, 1, -1 do
		local restored, result = pcall(function()
			local okWrite, writeError = writePropertyForSync(changed[index], "Archivable", false, ctx)
			if not okWrite then
				error(writeError, 0)
			end
		end)
		if not restored and restoreError == nil then
			restoreError = result
		end
	end
	if #nonArchivable > 0 then
		ctx.endStudioChangeSuppression()
	end
	if restoreError ~= nil then
		error(restoreError, 0)
	end
	if not ok then
		error(payload, 0)
	end
	return payload
end

local function mutationNeedsStructuralSnapshot(params: { [string]: any }, ctx: { [string]: any }): boolean
	if params.skipStructural == true then
		return false
	end
	if params.forceStructural == true or #(params.instanceChanges or {}) > 0 then
		return true
	end
	for _, change in ipairs(params.sourceChanges or {}) do
		local instance = resolveInstance(change, ctx, true)
		if instance == nil and change.deleted ~= true and liveHydrateEnabled(ctx) then
			return true
		end
		if
			instance ~= nil
			and instance.ClassName == "Folder"
			and ctx.luaSourceClass[tostring(change.className or "")]
		then
			return true
		end
	end
	for _, change in ipairs(params.propertyChanges or {}) do
		local instance = resolveInstance(change, ctx, true)
		local className = tostring(change.className or "")
		if instance ~= nil and className ~= "" and instance.ClassName ~= className then
			return true
		end
	end
	return false
end

local function captureSnapshotRoots(groups: { any }): ({ [string]: Instance }, { Instance }, { Instance }, number)
	local originalByPath = {}
	local originalRoots = {}
	local nonArchivable = {}
	local instanceCount = 0
	for _, group in ipairs(groups) do
		for _, child in ipairs(group.target:GetChildren()) do
			if not group.preserved[child] then
				originalRoots[#originalRoots + 1] = child
				local instances = { child }
				for _, descendant in ipairs(child:GetDescendants()) do
					instances[#instances + 1] = descendant
				end
				for _, instance in ipairs(instances) do
					instanceCount += 1
					if not instance.Archivable then
						nonArchivable[#nonArchivable + 1] = instance
					end
					local pathSegments, pathOrdinals = BridgeIdentity.getRefPathParts(instance)
					if pathSegments ~= nil then
						originalByPath[pathCacheKey(pathSegments, pathOrdinals)] = instance
					end
				end
			end
		end
	end
	return originalByPath, originalRoots, nonArchivable, instanceCount
end

local function captureMutationFingerprints(groups: { any }): { [string]: any }
	local fingerprints = {}
	for _, group in ipairs(groups) do
		local fingerprint = fingerprints[group.serviceName]
		if fingerprint == nil then
			fingerprint = { count = 0, entries = {} }
			fingerprints[group.serviceName] = fingerprint
		end
		local function record(instance: Instance)
			if fingerprint.entries[instance] == nil then
				fingerprint.count += 1
			end
			fingerprint.entries[instance] = { instance.Parent, instance.Name }
		end
		record(group.target)
		for _, child in ipairs(group.target:GetChildren()) do
			if not group.preserved[child] then
				record(child)
				for _, descendant in ipairs(child:GetDescendants()) do
					record(descendant)
				end
			end
		end
	end
	return fingerprints
end

local function mutationFingerprintsMatch(expected: { [string]: any }, actual: { [string]: any }): boolean
	for serviceName, expectedFingerprint in pairs(expected) do
		local actualFingerprint = actual[serviceName]
		if actualFingerprint == nil or actualFingerprint.count ~= expectedFingerprint.count then
			return false
		end
		for instance, expectedEntry in pairs(expectedFingerprint.entries) do
			local actualEntry = actualFingerprint.entries[instance]
			if
				actualEntry == nil
				or actualEntry[1] ~= expectedEntry[1]
				or actualEntry[2] ~= expectedEntry[2]
			then
				return false
			end
		end
	end
	return true
end

local function serializeMutationSnapshot(roots: { Instance }, nonArchivable: { Instance }, ctx: { [string]: any })
	if #roots == 0 then
		return nil
	end
	local okSerialize, serialized = pcall(serializeSnapshotRoots, roots, nonArchivable, ctx)
	if not okSerialize then
		error("Cannot create an editor rollback snapshot: " .. tostring(serialized))
	end
	return serialized
end

local TransactionState = {}

function TransactionState.captureServiceStates(serviceNames: { string }, groups: { any }, ctx: { [string]: any })
	local fingerprints = captureMutationFingerprints(groups)
	local generations = {}
	for _, serviceName in ipairs(serviceNames) do
		generations[serviceName] = ctx.studioChangeGeneration(serviceName)
	end
	return fingerprints, generations
end

function TransactionState.changedService(
	fingerprints: { [string]: any },
	generations: { [string]: any },
	groups: { any },
	ctx
): string?
	local currentFingerprints = captureMutationFingerprints(groups)
	if not mutationFingerprintsMatch(fingerprints, currentFingerprints) then
		for serviceName in pairs(fingerprints) do
			return serviceName
		end
	end
	for serviceName, generation in pairs(generations) do
		if ctx.studioChangeGeneration(serviceName) ~= generation then
			return serviceName
		end
	end
	return nil
end

function TransactionState.captureSources(sourceChanges: { any }, ctx: { [string]: any }): ({ any }, { [string]: boolean })
	local sources = {}
	local sourceKeys = {}
	local sourceSeen = {}
	for _, change in ipairs(sourceChanges) do
		sourceKeys[pathCacheKey(change.pathSegments, change.pathOrdinals)] = true
		local instance = resolveInstance(change, ctx, true)
		if instance ~= nil and ctx.luaSourceClass[instance.ClassName] and not sourceSeen[instance] then
			local okRead, source = readScriptSource(instance)
			if not okRead then
				error(`Could not snapshot Source for {instance:GetFullName()}: {source}`)
			end
			sourceSeen[instance] = true
			sources[#sources + 1] = { instance = instance, source = source }
		end
	end
	return sources, sourceKeys
end

function TransactionState.captureProperty(
	instance: Instance,
	propertyName: string,
	seenNames: { [string]: boolean },
	properties: { any },
	unreadablePropertyNames: { [Instance]: { [string]: boolean } }
)
	if seenNames[propertyName] then
		return
	end
	local okRead, value = readProperty(instance, propertyName)
	if okRead then
		seenNames[propertyName] = true
		properties[#properties + 1] = { instance = instance, name = propertyName, value = value }
		return
	end
	local unreadableNames = unreadablePropertyNames[instance]
	if unreadableNames == nil then
		unreadableNames = {}
		unreadablePropertyNames[instance] = unreadableNames
	end
	unreadableNames[propertyName] = true
end

function TransactionState.captureProperties(
	changes: { any },
	metadataTargets: { Instance },
	metadataSeen: { [Instance]: boolean },
	ctx: { [string]: any }
): ({ any }, { [Instance]: { [string]: boolean } })
	local properties = {}
	local propertySeen = {}
	local unreadablePropertyNames = {}
	for _, change in ipairs(changes) do
		local instance = resolveInstance(change, ctx, true)
		if instance ~= nil then
			addSnapshotMetadataTarget(metadataTargets, metadataSeen, instance)
			local seenNames = propertySeen[instance] or {}
			propertySeen[instance] = seenNames
			for rawPropertyName in pairs(change.properties or {}) do
				local propertyName = tostring(rawPropertyName)
				if propertyName ~= "Tags" then
					TransactionState.captureProperty(
						instance,
						propertyName,
						seenNames,
						properties,
						unreadablePropertyNames
					)
				end
			end
			for _, rawPropertyName in ipairs(change.resetProperties or {}) do
				TransactionState.captureProperty(
					instance,
					tostring(rawPropertyName),
					seenNames,
					properties,
					unreadablePropertyNames
				)
			end
		end
	end
	return properties, unreadablePropertyNames
end

function TransactionState.captureMetadata(metadataTargets: { Instance }): { any }
	local metadata = table.create(#metadataTargets)
	for index, instance in ipairs(metadataTargets) do
		metadata[index] = {
			instance = instance,
			attributes = instance:GetAttributes(),
			tags = CollectionService:GetTags(instance),
		}
	end
	return metadata
end

function TransactionState.captureSnapshot(serviceNames: { string }, params: { [string]: any }, ctx: { [string]: any })
	local hasStructuralChanges = mutationNeedsStructuralSnapshot(params, ctx)
	local groups, roots, metadataTargets, metadataSeen = {}, {}, {}, {}
	local fingerprintsByService, generationsByService = {}, {}
	if hasStructuralChanges then
		groups, roots, metadataTargets, metadataSeen =
			mutationSnapshotLayout(
				serviceNames,
				ctx,
				params.packageSnapshotRoots,
				params.mutationRootsByService,
				params.restrictedMutationServices
			)
		fingerprintsByService, generationsByService =
			TransactionState.captureServiceStates(serviceNames, groups, ctx)
	end
	local originalByPath, originalRoots, nonArchivable, instanceCount = captureSnapshotRoots(groups)
	local payload = serializeMutationSnapshot(roots, nonArchivable, ctx)
	local changedService = TransactionState.changedService(
		fingerprintsByService,
		generationsByService,
		groups,
		ctx
	)
	if changedService ~= nil then
		local attempt = tonumber(params.snapshotAttempt) or 0
		if attempt < 2 then
			local retryParams = table.clone(params)
			retryParams.snapshotAttempt = attempt + 1
			return TransactionState.captureSnapshot(serviceNames, retryParams, ctx)
		end
		error(`Studio kept changing {changedService} while Renium prepared rollback data`)
	end

	local sources, sourceKeys = TransactionState.captureSources(params.sourceChanges or {}, ctx)
	local properties, unreadablePropertyNames = TransactionState.captureProperties(
		params.propertyChanges or {},
		metadataTargets,
		metadataSeen,
		ctx
	)
	return {
		groups = groups,
		payload = payload,
		rootCount = #roots,
		metadata = TransactionState.captureMetadata(metadataTargets),
		properties = properties,
		sources = sources,
		unreadablePropertyNames = unreadablePropertyNames,
		originalByPath = originalByPath,
		originalRoots = originalRoots,
		instanceCount = instanceCount,
		currentCamera = Workspace.CurrentCamera,
		scriptDocuments = ScriptDocumentState.capture(
			serviceNames,
			if hasStructuralChanges or params.captureAllScriptDocuments == true then nil else sourceKeys
		),
		referenceOverlay = ReferenceOverlay.capture(groups),
		fingerprintsByService = fingerprintsByService,
	}
end

function TransactionState.restoreMetadata(
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

function TransactionState.topologyMatchesSnapshot(snapshot: { [string]: any }): boolean
	local fingerprints = snapshot.fingerprintsByService or {}
	if next(fingerprints) == nil then
		return false
	end
	return mutationFingerprintsMatch(fingerprints, captureMutationFingerprints(snapshot.groups or {}))
end

function TransactionState.restoreSnapshotState(
	snapshot: { [string]: any },
	replacements: { [Instance]: Instance },
	ctx: { [string]: any }
)
	TransactionState.restoreMetadata(snapshot, replacements, ctx)
	if snapshot.currentCamera ~= nil then
		setCurrentCameraForSync(replacements[snapshot.currentCamera] or snapshot.currentCamera, ctx)
	end
	ScriptDocumentState.apply(snapshot.scriptDocuments or {}, nil, nil, replacements)
	ReferenceOverlay.apply(snapshot.referenceOverlay or {}, replacements, ctx)
end

function TransactionState.restoreSnapshot(
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
	local instanceCount = 0
	for _, root in ipairs(roots) do
		instanceCount += 1 + #root:GetDescendants()
	end
	if instanceCount ~= snapshot.instanceCount then
		error(
			`Editor rollback snapshot is incomplete: expected {snapshot.instanceCount} instances, got {instanceCount}`
		)
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
	local removed = BridgeInstanceSwap.replaceChildren(snapshot.groups, incomingByGroup, function(instance, parent)
		setParentForSync(instance, parent, ctx)
	end)
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
	TransactionState.restoreSnapshotState(snapshot, replacements, ctx)
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

function TransactionState.appendJournalRecords(session: { [string]: any }, records: { any })
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

function TransactionState.drainJournal(session: { [string]: any }, ctx: { [string]: any }): { any }
	if not session.journalActive then
		return {}
	end
	local records = ctx.drainStudioChangeJournal(session.transactionId)
	TransactionState.appendJournalRecords(session, records)
	return records
end

function TransactionState.finishJournal(session: { [string]: any }, ctx: { [string]: any }): { any }
	if session.journalActive then
		local records = ctx.finishStudioChangeJournal(session.transactionId)
		session.journalActive = false
		TransactionState.appendJournalRecords(session, records)
	end
	return session.changeJournal or {}
end

function TransactionState.instanceWillBeRemovedByRollback(instance: Instance, session: { [string]: any }): boolean
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

function TransactionState.prepareJournalRollback(session: { [string]: any }, records: { any }, ctx: { [string]: any })
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
			and TransactionState.instanceWillBeRemovedByRollback(instance, session)
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

function TransactionState.resolveJournalParent(record: { [string]: any }, replacements: { [Instance]: Instance }): Instance?
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

function TransactionState.markJournalServices(record: { [string]: any }, changedServices: { [string]: boolean })
	for serviceName in pairs(record.services or { [record.service] = true }) do
		changedServices[serviceName] = true
	end
end

function TransactionState.replayJournalPlacement(
	record: { [string]: any },
	replacements: { [Instance]: Instance },
	ctx: { [string]: any }
): boolean
	local instance = resolveReplacement(record.currentInstance, replacements)
	if instance == nil then
		return false
	end
	local parent = TransactionState.resolveJournalParent(record, replacements)
	if (record.structural or (instance.Parent == nil and parent ~= nil)) and instance.Parent ~= parent then
		setParentForSync(instance, parent, ctx)
	end
	return true
end

function TransactionState.remapJournalValue(value: any, replacements: { [Instance]: Instance }): any
	if typeof(value) == "Instance" then
		return resolveReplacement(value, replacements) or value
	end
	if typeof(value) == "Content" and value.SourceType == Enum.ContentSourceType.Object and value.Object ~= nil then
		local target = resolveReplacement(value.Object, replacements)
		if target ~= nil then
			return Content.fromObject(target)
		end
	end
	return value
end

function TransactionState.replayJournalProperty(
	instance: Instance,
	propertyName: string,
	value: any,
	ctx: { [string]: any }
)
	local sourceProperty = propertyName == "Source" and instance:IsA("LuaSourceContainer")
	local okCurrent, current
	if sourceProperty then
		okCurrent, current = readScriptSource(instance)
	else
		okCurrent, current = readProperty(instance, propertyName)
	end
	if okCurrent and valuesEqual(current, value) then
		return
	end
	local okWrite, writeError
	if sourceProperty then
		okWrite, writeError = setSource(instance, value, ctx)
	else
		okWrite, writeError = writePropertyForSync(instance, propertyName, value, ctx)
	end
	if not okWrite then
		error(`Could not preserve concurrent Studio edit to {instance:GetFullName()}.{propertyName}: {writeError}`)
	end
end

function TransactionState.replayJournalAttributes(
	instance: Instance,
	attributes: { [string]: any },
	ctx: { [string]: any }
)
	for attributeName, entry in pairs(attributes) do
		if entry.captured and not valuesEqual(instance:GetAttribute(attributeName), entry.value) then
			local okWrite, writeError = setAttributeForSync(instance, attributeName, entry.value, ctx)
			if not okWrite then
				error(
					`Could not preserve concurrent Studio edit to {instance:GetFullName()}.{attributeName}: {writeError}`
				)
			end
		end
	end
end

function TransactionState.replayJournalTags(instance: Instance, tags: { string }, ctx: { [string]: any })
	local desired = {}
	for _, tag in ipairs(tags) do
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

function TransactionState.replayJournalValues(
	record: { [string]: any },
	replacements: { [Instance]: Instance },
	ctx: { [string]: any }
)
	local instance = resolveReplacement(record.currentInstance, replacements)
	if instance == nil or (instance.Parent == nil and instance ~= game:GetService(record.service)) then
		return
	end
	for propertyName, entry in pairs(record.properties) do
		if entry.captured then
			TransactionState.replayJournalProperty(
				instance,
				propertyName,
				TransactionState.remapJournalValue(entry.value, replacements),
				ctx
			)
		end
	end
	TransactionState.replayJournalAttributes(instance, record.attributes, ctx)
	if record.tags ~= nil then
		TransactionState.replayJournalTags(instance, record.tags, ctx)
	end
end

function TransactionState.replayJournal(
	records: { any },
	replacements: { [Instance]: Instance },
	ctx: { [string]: any }
): { [string]: boolean }
	local changedServices = {}
	for _, record in ipairs(records) do
		if TransactionState.replayJournalPlacement(record, replacements, ctx) then
			TransactionState.markJournalServices(record, changedServices)
		end
	end
	for _, record in ipairs(records) do
		TransactionState.replayJournalValues(record, replacements, ctx)
	end
	return changedServices
end

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
	local snapshotReplacements = {}
	if TransactionState.topologyMatchesSnapshot(session.snapshot) then
		if beforeReplace ~= nil then
			beforeReplace()
		end
		TransactionState.restoreSnapshotState(session.snapshot, replacements, ctx)
	else
		snapshotReplacements =
			TransactionState.restoreSnapshot(session.snapshot, ctx, session.instanceReplacements, beforeReplace)
	end
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

function TransactionState.rollbackSession(session: { [string]: any }, ctx: { [string]: any }): { [Instance]: Instance }
	finishHistoryRecording(session.historyRecording, Enum.FinishRecordingOperation.Cancel)
	session.historyRecording = nil
	if session.mutated ~= true then
		TransactionState.finishJournal(session, ctx)
		session.changeJournal = nil
		if session.nativeUndo ~= nil then
			local incoming = ReferenceOverlay.rollbackNative(session.nativeUndo, ctx)
			session.nativeUndo = nil
			for _, instance in ipairs(incoming) do
				if instance.Parent == nil then
					instance:Destroy()
				end
			end
		end
		return {}
	end
	local ok, result = xpcall(function()
		local initialRecords = TransactionState.drainJournal(session, ctx)
		TransactionState.prepareJournalRollback(session, initialRecords, ctx)
		local function preservePendingJournalChanges()
			local records = TransactionState.drainJournal(session, ctx)
			TransactionState.prepareJournalRollback(session, records, ctx)
		end
		local replacements = TransactionState.rollback(session, ctx, preservePendingJournalChanges)
		local records = TransactionState.finishJournal(session, ctx)
		TransactionState.replayJournal(records, replacements, ctx)
		if session.nativeUndo ~= nil then
			local stableFrames = 0
			local firstError
			for _ = 1, 4 do
				RunService.Heartbeat:Wait()
				local stable = true
				for _, group in ipairs(session.nativeUndo.prepared) do
					for _, instance in ipairs(group.outgoing) do
						if instance.Parent ~= group.target then
							stable = false
							local restored, restoreError = pcall(setParentForSync, instance, group.target, ctx)
							if not restored then
								firstError = firstError or restoreError
							end
						end
					end
					for _, instance in ipairs(group.retainedLiveRoots or {}) do
						if instance.Parent ~= group.target then
							stable = false
							local restored, restoreError = pcall(setParentForSync, instance, group.target, ctx)
							if not restored then
								firstError = firstError or restoreError
							end
						end
					end
				end
				if stable then
					stableFrames += 1
					if stableFrames == 2 then
						break
					end
				else
					stableFrames = 0
				end
			end
			if stableFrames < 2 then
				error(`Editor rollback did not stabilize native roots: {firstError or "roots remained detached"}`)
			end
		end
		for _, originalRoot in ipairs(session.snapshot.originalRoots or {}) do
			local restoredRoot = resolveReplacement(originalRoot, replacements)
			if restoredRoot == nil or restoredRoot.Parent == nil then
				error(`Editor rollback did not restore {originalRoot.Name}`)
			end
		end
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

local NativeSerialization = {}

function NativeSerialization.updateStatus(session: { [string]: any })
	if session.serializationScheduleReady and session.pendingPayloads == 0 then
		session.status = if session.error or next(session.payloadErrors) then "failed" else "ready"
	end
end

function NativeSerialization.completePayload(
	session: { [string]: any },
	payloadKey: string?,
	generations: { [string]: number }?,
	ok: boolean,
	payload: any
)
	if session.cancelled then
		session.pendingPayloads = math.max(0, session.pendingPayloads - 1)
		session.updatedAt = os.clock()
		session.payloadReadyEvent:Fire()
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
			local payloadHash = buffer.tostring(
				EncodingService:Base64Encode(EncodingService:ComputeBufferHash(payload, Enum.HashAlgorithm.Blake3))
			)
			session.payloads[payloadKey] = payload
			session.payloadHashes[payloadKey] = payloadHash
			if generations ~= nil then
				local unchanged = true
				for serviceName, generation in pairs(generations) do
					if
						not session.ctx.isStudioChangeTracking(serviceName)
						or session.ctx.studioChangeGeneration(serviceName) ~= generation
					then
						unchanged = false
						break
					end
				end
				if unchanged then
					nativePayloadProofs[payloadKey] = {
						payloadHash = payloadHash,
						totalBytes = totalBytes,
						generations = generations,
					}
				end
			end
		else
			session.payload = payload
			session.totalBytes = totalBytes
			session.payloadHash = buffer.tostring(
				EncodingService:Base64Encode(EncodingService:ComputeBufferHash(payload, Enum.HashAlgorithm.Blake3))
			)
		end
	end
	session.pendingPayloads -= 1
	NativeSerialization.updateStatus(session)
	session.updatedAt = os.clock()
	session.payloadReadyEvent:Fire()
end

local NO_INSTANCES = {}

function NativeSerialization.runJob(session: { [string]: any }, job: { [string]: any })
	if session.cancelled then
		return
	end
	session.activeSerializations += 1
	local nonArchivableInstances = job.nonArchivableInstances or NO_INSTANCES
	local ok, payload = xpcall(function()
		return serializeSnapshotRoots(job.roots, nonArchivableInstances, session.ctx)
	end, debug.traceback)
	session.activeSerializations -= 1
	NativeSerialization.completePayload(session, job.payloadKey, job.generations, ok, payload)
	if session.snapshotCleanupPending and session.activeSerializations == 0 then
		session.cleanupSnapshot()
	end
end

function NativeSerialization.captureGenerations(
	session: { [string]: any },
	services: { string }
): { [string]: number }?
	local generations = {}
	for _, serviceName in ipairs(services) do
		if not session.ctx.isStudioChangeTracking(serviceName) then
			return nil
		end
		generations[serviceName] = session.ctx.studioChangeGeneration(serviceName)
	end
	return generations
end

function NativeSerialization.proofMatches(session: { [string]: any }, proof: { [string]: any }): boolean
	for serviceName, generation in pairs(proof.generations) do
		if
			not session.ctx.isStudioChangeTracking(serviceName)
			or session.ctx.studioChangeGeneration(serviceName) ~= generation
		then
			return false
		end
	end
	return true
end

function NativeSerialization.generationsMatch(
	left: { [string]: number },
	right: { [string]: number }
): boolean
	for serviceName, generation in pairs(left) do
		if right[serviceName] ~= generation then
			return false
		end
	end
	for serviceName in pairs(right) do
		if left[serviceName] == nil then
			return false
		end
	end
	return true
end

function NativeSerialization.invalidateProofs(serviceName: string)
	for payloadKey, proof in pairs(nativePayloadProofs) do
		if proof.generations[serviceName] ~= nil then
			nativePayloadProofs[payloadKey] = nil
		end
	end
end

function NativeSerialization.materializeCachedPayload(session: { [string]: any }, payloadKey: string)
	local cached = session.cachedPayloadProofs[payloadKey]
	if cached == nil then
		return
	end
	if not NativeSerialization.proofMatches(session, cached.proof) then
		nativePayloadProofs[payloadKey] = nil
		error("Studio changed during native export")
	end
	session.cachedPayloadProofs[payloadKey] = nil
	session.payloads[payloadKey] = nil
	session.payloadHashes[payloadKey] = nil
	session.pendingPayloads += 1
	session.status = "pending"
	NativeSerialization.runJob(session, cached.job)
end

function NativeSerialization.startWorkers(session: { [string]: any }, workerCount: number)
	local nextSerializationIndex = 0
	for _ = 1, workerCount do
		task.spawn(function()
			while not session.cancelled do
				local job = session.serializationJobs[nextSerializationIndex + 1]
				if job ~= nil then
					nextSerializationIndex += 1
					NativeSerialization.runJob(session, job)
			elseif session.serializationScheduleReady then
				break
			else
				session.payloadReadyEvent.Event:Wait()
			end
		end
		end)
	end
end

function NativeSerialization.appendJob(
	session: { [string]: any },
	payloadKey: string,
	services: { string },
	roots: { Instance },
	nonArchivableInstances: { Instance }
)
	for _, instance in ipairs(nonArchivableInstances) do
		session.originalNonArchivableInstances[instance] = true
	end
	local job = {
		payloadKey = payloadKey,
		generations = NativeSerialization.captureGenerations(session, services),
		roots = roots,
		nonArchivableInstances = nonArchivableInstances,
	}
	local proof = nativePayloadProofs[payloadKey]
	if
		job.generations ~= nil
		and proof ~= nil
		and NativeSerialization.generationsMatch(job.generations, proof.generations)
		and NativeSerialization.proofMatches(session, proof)
	then
		session.cachedPayloadProofs[payloadKey] = {
			job = job,
			proof = proof,
		}
		session.payloadHashes[payloadKey] = proof.payloadHash
		session.payloadSizes[payloadKey] = proof.totalBytes
		return
	end
	session.serializationJobs[#session.serializationJobs + 1] = job
	session.pendingPayloads += 1
	session.payloadReadyEvent:Fire()
end

function NativeSerialization.nonArchivableInstances(session: { [string]: any }, services: { string }): { Instance }
	local instances = {}
	for _, serviceName in ipairs(services) do
		for _, instance in ipairs(session.nonArchivableByService[serviceName] or NO_INSTANCES) do
			instances[#instances + 1] = instance
		end
	end
	return instances
end

function NativeSerialization.appendIdentityCarriers(
	group: { [string]: any },
	roots: { Instance },
	instances: { Instance },
	structureGeneration: number?
)
	local pool = nativeSnapshotPools[group.service]
	local prefix = `__ReniumNativeIdentity:{group.service}:`
	local carrierCount = math.ceil(math.max(#instances - 1, 0) / #NATIVE_IDENTITY_CARRIER_SLOTS)
	local canReuse = structureGeneration ~= nil
		and pool.identityStructureGeneration == structureGeneration
		and pool.identityInstanceCount == #instances
	if not canReuse then
		local firstChangedIndex = 2
		local previousInstances = pool.identityInstances
		if previousInstances ~= nil then
			local commonCount = math.min(#previousInstances, #instances)
			while firstChangedIndex <= commonCount and previousInstances[firstChangedIndex] == instances[firstChangedIndex] do
				firstChangedIndex += 1
			end
		end
		local firstCarrierIndex = math.floor(math.max(firstChangedIndex - 2, 0) / #NATIVE_IDENTITY_CARRIER_SLOTS) + 1
		local instanceIndex = (firstCarrierIndex - 1) * #NATIVE_IDENTITY_CARRIER_SLOTS + 2
		for carrierIndex = firstCarrierIndex, carrierCount do
			local carrier = pool.carriers[carrierIndex]
			if carrier == nil then
				carrier = Instance.new(NATIVE_IDENTITY_CARRIER_CLASS)
				carrier.Name = prefix .. tostring(carrierIndex)
				pool.carriers[carrierIndex] = carrier
			end
			local writableCarrier = carrier :: any
			for _, propertyName in ipairs(NATIVE_IDENTITY_CARRIER_SLOTS) do
				local target = instances[instanceIndex]
				if writableCarrier[propertyName] ~= target then
					writableCarrier[propertyName] = target
				end
				if target == nil then
					continue
				end
				instanceIndex += 1
			end
		end
		pool.identityStructureGeneration = structureGeneration
		pool.identityInstanceCount = #instances
		pool.identityInstances = instances
	end
	for carrierIndex = 1, carrierCount do
		roots[#roots + 1] = pool.carriers[carrierIndex] :: Instance
	end
	group.identityCarrierClass = NATIVE_IDENTITY_CARRIER_CLASS
	group.identityCarrierPrefix = prefix
	group.identityCarrierSlots = NATIVE_IDENTITY_CARRIER_SLOTS
	group.identityCarrierCount = carrierCount
end

function NativeSerialization.snapshotMarker(serviceName: string, service: Instance): Instance
	local pool = nativeSnapshotPools[serviceName]
	if pool == nil then
		pool = {
			marker = Instance.new("Folder"),
			carriers = {},
		}
		pool.marker.Name = serviceName
		nativeSnapshotPools[serviceName] = pool
	end
	local marker = pool.marker
	local attributes = service:GetAttributes()
	for name, value in pairs(marker:GetAttributes()) do
		if name:sub(1, 3) == "RBX" or attributes[name] == nil then
			marker:SetAttribute(name, nil)
		elseif attributes[name] == value then
			attributes[name] = nil
		end
	end
	for name, value in pairs(attributes) do
		if name:sub(1, 3) ~= "RBX" then
			marker:SetAttribute(name, value)
		end
	end
	local tags = {}
	for _, tag in ipairs(CollectionService:GetTags(service)) do
		tags[tag] = true
		if not CollectionService:HasTag(marker, tag) then
			CollectionService:AddTag(marker, tag)
		end
	end
	for _, tag in ipairs(CollectionService:GetTags(marker)) do
		if not tags[tag] then
			CollectionService:RemoveTag(marker, tag)
		end
	end
	return marker
end

function NativeSerialization.appendGroups(
	session: { [string]: any },
	groups: { any },
	rootsByService: { [string]: { Instance } }
)
	if #groups == 1 then
		local group = groups[1]
		local roots = rootsByService[group.service]
		NativeSerialization.appendJob(
			session,
			group.service,
			{ group.service },
			roots,
			NativeSerialization.nonArchivableInstances(session, { group.service })
		)
		return
	end

	local roots = {}
	local services = table.create(#groups)
	for index, group in ipairs(groups) do
		local groupRoots = rootsByService[group.service]
		services[index] = group.service
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
	NativeSerialization.appendJob(
		session,
		batchId,
		services,
		roots,
		NativeSerialization.nonArchivableInstances(session, services)
	)
end

function NativeSerialization.finishSchedule(
	session: { [string]: any },
	groups: { any },
	rootsByService: { [string]: { Instance } },
	startIndex: number
)
	local pendingGroups = {}
	local pendingInstanceCount = 0
	local function flushPendingGroups()
		if #pendingGroups > 0 then
			NativeSerialization.appendGroups(session, pendingGroups, rootsByService)
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
			NativeSerialization.appendJob(
				session,
				group.service,
				{ group.service },
				roots,
				NativeSerialization.nonArchivableInstances(session, { group.service })
			)
		end
	end
	flushPendingGroups()
	session.serializationScheduleReady = true
	NativeSerialization.updateStatus(session)
	session.payloadReadyEvent:Fire()
end

function BridgeEditorSync.create(ctx: { [string]: any })
	local api = {}
	local cancellationGeneration = 0
	local activeOperation = nil
	local currentRequestLeaseByThread = {}
	local cancelledLeaseAllowedByThread = {}
	local activeRequestLeaseCounts = {}
	local cancelledRequestLeases = {}
	local transactionOutcomes = BridgeConnection.createTransactionOutcomeStore(
		TRANSACTION_OUTCOME_TTL_SECONDS,
		MAX_TRANSACTION_OUTCOMES
	)
	local filterCandidateSnapshots = {}
	local filterCandidateSnapshotCounter = 0
	local filterCandidateSnapshotTtlSeconds = 15
	local maxFilterCandidateSnapshots = 8
	local function invalidateEditorService(serviceName: string)
		NativeSerialization.invalidateProofs(serviceName)
		ctx.invalidateService(serviceName)
	end

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

	local function beginTrackedOperation(kind: string)
		local now = os.clock()
		local operation = {
			kind = kind,
			phase = "starting",
			startedAt = now,
			phaseStartedAt = now,
		}
		activeOperation = operation
		return operation
	end

	local function setTrackedOperationPhase(operation, phase: string)
		if activeOperation == operation then
			operation.phase = phase
			operation.phaseStartedAt = os.clock()
		end
	end

	local function finishTrackedOperation(operation)
		if activeOperation == operation then
			activeOperation = nil
		end
	end

	function api.operationState(): { [string]: any }?
		local operation = activeOperation
		if operation == nil then
			return nil
		end
		local now = os.clock()
		return {
			kind = operation.kind,
			phase = operation.phase,
			elapsedMs = (now - operation.startedAt) * 1000,
			phaseMs = (now - operation.phaseStartedAt) * 1000,
		}
	end

	local function pruneCancelledRequestLeases()
		local now = os.clock()
		for leaseId, expiresAt in pairs(cancelledRequestLeases) do
			if expiresAt <= now then
				cancelledRequestLeases[leaseId] = nil
			end
		end
	end

	local function currentRequestLeaseId(): string?
		return currentRequestLeaseByThread[coroutine.running()]
	end

	local function assertRequestLeaseActive(leaseId: string?)
		pruneCancelledRequestLeases()
		local thread = coroutine.running()
		if
			leaseId ~= nil
			and cancelledRequestLeases[leaseId] ~= nil
			and cancelledLeaseAllowedByThread[thread] ~= leaseId
		then
			error("Renium request lease was cancelled")
		end
	end

	function api.assertCurrentRequestLeaseActive()
		assertRequestLeaseActive(currentRequestLeaseId())
	end

	function api.withRequestLease(leaseId: string?, allowCancelled: boolean, operation, ...)
		if leaseId ~= nil and (type(leaseId) ~= "string" or leaseId == "" or #leaseId > 128) then
			error("Invalid request lease id")
		end
		local thread = coroutine.running()
		local previousLeaseId = currentRequestLeaseByThread[thread]
		local previousCancelledLease = cancelledLeaseAllowedByThread[thread]
		local arguments = table.pack(...)
		currentRequestLeaseByThread[thread] = leaseId
		cancelledLeaseAllowedByThread[thread] = if allowCancelled and leaseId ~= nil then leaseId else nil
		if leaseId ~= nil then
			activeRequestLeaseCounts[leaseId] = (activeRequestLeaseCounts[leaseId] or 0) + 1
		end
		local results = table.pack(xpcall(function()
			assertRequestLeaseActive(leaseId)
			local operationResults = table.pack(operation(table.unpack(arguments, 1, arguments.n)))
			if not allowCancelled then
				assertRequestLeaseActive(leaseId)
			end
			return table.unpack(operationResults, 1, operationResults.n)
		end, debug.traceback))
		if leaseId ~= nil then
			local remaining = activeRequestLeaseCounts[leaseId] - 1
			activeRequestLeaseCounts[leaseId] = if remaining > 0 then remaining else nil
		end
		currentRequestLeaseByThread[thread] = previousLeaseId
		cancelledLeaseAllowedByThread[thread] = previousCancelledLease
		if not results[1] then
			error(results[2], 0)
		end
		return table.unpack(results, 2, results.n)
	end

	local function assertTransactionLease(session: { [string]: any })
		local leaseId = currentRequestLeaseId()
		if session.leaseId ~= nil and session.leaseId ~= leaseId then
			error("Editor transaction belongs to another request lease")
		end
		assertRequestLeaseActive(session.leaseId or leaseId)
	end

	local function recordTransactionOutcome(
		transactionId: string,
		state: string,
		response: { [string]: any }
	): { [string]: any }
		local outcome = transactionOutcomes.record(transactionId, state, response)
		ctx.finishEditorTransactionExpectation(transactionId)
		return outcome
	end

	function api.getTransactionState(params: { [string]: any }): { [string]: any }
		local transactionId = tostring(params.transactionId or "")
		if transactionId == "" then
			error("Invalid editor transaction id")
		end
		local session = editorTransactions[transactionId]
		if type(session) == "table" then
			local state = if session.rollbackFailed ~= nil then "rollbackFailed" else tostring(session.state or "open")
			return {
				ok = true,
				found = true,
				transactionId = transactionId,
				state = state,
				committed = false,
				rolledBack = false,
				rollbackError = session.rollbackFailed,
			}
		end
		local outcome = transactionOutcomes.get(transactionId)
		if outcome ~= nil then
			return outcome
		end
		return {
			ok = true,
			found = false,
			transactionId = transactionId,
			state = "notFound",
			committed = false,
			rolledBack = false,
		}
	end

	function api.cancelRequestLease(leaseId: string): { [string]: any }
		if leaseId == "" or #leaseId > 128 then
			error("Invalid request lease id")
		end
		pruneCancelledRequestLeases()
		cancelledRequestLeases[leaseId] = os.clock() + REQUEST_LEASE_CANCELLATION_TTL_SECONDS
		for _, session in pairs(binaryExports) do
			if type(session) == "table" and session.leaseId == leaseId then
				session.cancelled = true
				session.payloadReadyEvent:Fire()
			end
		end
		for _, session in pairs(binaryImports) do
			if type(session) == "table" and session.leaseId == leaseId then
				session.cancelRequested = true
			end
		end
		for _, session in pairs(reconcileSessions) do
			if type(session) == "table" and session.leaseId == leaseId then
				session.cancelRequested = true
			end
		end
		for _, session in pairs(editorTransactions) do
			if type(session) == "table" and session.leaseId == leaseId and not session.commitFence then
				session.cancelRequested = true
			end
		end
		return {
			ok = true,
			active = (activeRequestLeaseCounts[leaseId] or 0) > 0,
		}
	end

	function api.finishRequestLeaseCancellation(leaseId: string): { [string]: any }
		local rolledBack = 0
		local transactionIds = {}
		for exportId, session in pairs(binaryExports) do
			if type(session) == "table" and session.leaseId == leaseId then
				session.cancelled = true
				session.payloadReadyEvent:Fire()
				while session.activeSerializations > 0 do
					session.payloadReadyEvent.Event:Wait()
				end
				session.expireRequested = true
				expireSession(binaryExports, exportId, session)
			end
		end
		for importId, session in pairs(binaryImports) do
			if type(session) == "table" and session.leaseId == leaseId then
				session.cancelRequested = true
				session.expireRequested = true
				expireSession(binaryImports, importId, session)
			end
		end
		for sessionKey, session in pairs(reconcileSessions) do
			if type(session) == "table" and session.leaseId == leaseId then
				session.cancelRequested = true
				session.expireRequested = true
				expireSession(reconcileSessions, sessionKey, session)
			end
		end
		for transactionId, session in pairs(editorTransactions) do
			if
				type(session) == "table"
				and session.leaseId == leaseId
				and not session.commitFence
				and (session.activeOperations or 0) == 0
			then
				local ok, replacements = pcall(runWithStudioChangeSuppression, ctx, function()
					return TransactionState.rollbackSession(session, ctx)
				end)
				if ok then
					session.onExpire = nil
					editorTransactions[transactionId] = nil
					recordTransactionOutcome(transactionId, "rolledBack", {
						replacements = countEntries(replacements),
					})
					for _, serviceName in ipairs(session.serviceNames) do
						invalidateEditorService(serviceName)
					end
					rolledBack += 1
					transactionIds[#transactionIds + 1] = transactionId
				else
					armSessionExpiry(editorTransactions, transactionId, session)
					warn("[Renium] request lease rollback failed: " .. tostring(replacements))
				end
			end
		end
		return {
			ok = true,
			rolledBack = rolledBack,
			transactionIds = transactionIds,
		}
	end

	function api.matchedSettingsId(instance: Instance): string?
		local byInstance = ctx.matchedSettingsIdByInstance
		if type(byInstance) ~= "table" or liveInstance(instance) == nil then
			return nil
		end
		return settingsIdText(byInstance[instance])
	end

	function api.matchedSettingsIdVersion(): number
		return tonumber(ctx.matchedSettingsIdVersion) or 0
	end

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

	local function captureOperationCancellation()
		return {
			generation = cancellationGeneration,
			leaseId = currentRequestLeaseId(),
		}
	end

	local function assertSessionOwnership(operationGeneration)
		local generation = if type(operationGeneration) == "table"
			then operationGeneration.generation
			else operationGeneration
		if generation ~= nil and generation ~= cancellationGeneration then
			error("Renium operation was cancelled")
		end
		local leaseId = if type(operationGeneration) == "table"
			then operationGeneration.leaseId
			else currentRequestLeaseId()
		assertRequestLeaseActive(leaseId)
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
		local okRestore, replacements = pcall(TransactionState.restoreSnapshot, snapshot, ctx, nil)
		if not okRestore then
			error("Could not roll back the editor reconcile: " .. tostring(replacements))
		end
		restoreExplorerSelection(selected, replacements)
		invalidateEditorService(serviceName)
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

	local function hasDirectPackageLink(instance: Instance): boolean
		return instance:FindFirstChildWhichIsA("PackageLink") ~= nil
	end

	local function directPackageVersion(instance: Instance): number
		local packageLink = instance:FindFirstChildWhichIsA("PackageLink")
		if packageLink == nil then
			error("Package root no longer contains a PackageLink")
		end
		local ok, version = readProperty(packageLink, "VersionNumber")
		if not ok or type(version) ~= "number" or version <= 0 or version % 1 ~= 0 then
			error("PackageLink.VersionNumber is unavailable")
		end
		return version
	end

	local function tagsWouldChange(instance: Instance, rawTags: any): boolean
		local desired = {}
		for _, tag in pairs(if type(rawTags) == "table" then rawTags else {}) do
			if type(tag) == "string" and tag ~= "" then
				desired[tag] = true
			end
		end
		for _, tag in ipairs(CollectionService:GetTags(instance)) do
			if not desired[tag] then
				return true
			end
			desired[tag] = nil
		end
		return next(desired) ~= nil
	end

	local function propertyChangeWouldMutate(change: { [string]: any }): boolean
		local instance = resolveInstance(change, ctx)
		if instance == nil then
			return true
		end
		if isProtectedWorkspaceCameraPath(change.pathSegments) or isProtectedWorkspaceCameraInstance(instance) then
			return false
		end
		local serviceName = tostring(change.service or "")
		for _, propertyName in ipairs(changedPropertyNames(change.properties or {})) do
			local rawValue = change.properties[propertyName]
			if propertyName == "Name" then
				if instance.Name ~= tostring(rawValue) then
					return true
				end
			elseif propertyName == "Tags" then
				if tagsWouldChange(instance, rawValue) then
					return true
				end
			elseif propertyName ~= "Source" then
				local okDecode, decoded = decodePropertyValue(instance, propertyName, rawValue, ctx, serviceName)
				local okRead, current = readProperty(instance, propertyName)
				if not okDecode or not okRead or not valuesEqual(current, decoded) then
					return true
				end
			end
		end
		for _, propertyName in ipairs(change.resetProperties or {}) do
			local okCreate, defaultInstance = pcall(Instance.new, instance.ClassName)
			if not okCreate or defaultInstance == nil then
				return true
			end
			local okDefault, defaultValue = readProperty(defaultInstance, propertyName)
			defaultInstance:Destroy()
			local okRead, current = readProperty(instance, propertyName)
			if not okDefault or not okRead or not valuesEqual(current, defaultValue) then
				return true
			end
		end
		for _, attributeName in ipairs(change.deletedAttributes or {}) do
			if instance:GetAttribute(attributeName) ~= nil then
				return true
			end
		end
		for attributeName, rawValue in pairs(change.attributes or {}) do
			local okDecode, decoded = decodeValue(rawValue, nil, ctx, serviceName)
			if not okDecode or not valuesEqual(instance:GetAttribute(attributeName), decoded) then
				return true
			end
		end
		return false
	end

	local function activeMutationPackageTargets(params: { [string]: any }): { any }
		local activePropertyPaths = {}
		for _, change in ipairs(params.propertyChanges or {}) do
			if propertyChangeWouldMutate(change) then
				activePropertyPaths[pathCacheKey(change.pathSegments, change.pathOrdinals)] = true
			end
		end
		local targets = {}
		for _, target in ipairs(params.mutationPackageTargets or {}) do
			if
				target.kind ~= "property"
				or activePropertyPaths[pathCacheKey(target.pathSegments, target.pathOrdinals)]
			then
				targets[#targets + 1] = target
			end
		end
		return targets
	end

	local function mutationPackages(rawTargets: any, maxTargets: number): { any }
		local targetsAreArray, targetCount = denseArrayLength(rawTargets)
		if
			not targetsAreArray
			or targetCount > math.min(maxTargets, tonumber(ctx.maxChangesPerRequest) or 5000)
		then
			error("Invalid editor mutation package request")
		end
		local packages = {}
		local packageKeys = {}
		for index, descriptor in ipairs(rawTargets) do
			if type(descriptor) ~= "table" then
				error(`Editor mutation package target {index} must be an object`)
			end
			local serviceName = tostring(descriptor.service or "")
			if not ctx.allowedServices[serviceName] then
				error(`Editor mutation package target {index} has an invalid service`)
			end
			validateMutationPath(descriptor, serviceName, `Editor mutation package target {index}`, ctx)
			local pathSegments = table.clone(descriptor.pathSegments)
			local pathOrdinals = cloneArray(descriptor.pathOrdinals)
			local exact = true
			local instance = resolvePathSegments(pathSegments, nil, pathOrdinals)
			while instance == nil and #pathSegments > 1 do
				exact = false
				table.remove(pathSegments)
				table.remove(pathOrdinals)
				instance = resolvePathSegments(pathSegments, nil, pathOrdinals)
			end
			if exact and instance ~= nil and instance:IsA("PackageLink") then
				error("PackageLink instances are read-only")
			end
			if exact and descriptor.includeSelf ~= true and instance ~= nil then
				instance = instance.Parent
			end
			local service = game:GetService(serviceName)
			while instance ~= nil and instance ~= service do
				if hasDirectPackageLink(instance) then
					local packageSegments, packageOrdinals = BridgeIdentity.getRefPathParts(instance)
					if packageSegments == nil then
						error("Package root has no stable Studio path")
					end
					local key = pathCacheKey(packageSegments, packageOrdinals)
					if not packageKeys[key] then
						packageKeys[key] = true
						packages[#packages + 1] = {
							pathSegments = packageSegments,
							pathOrdinals = packageOrdinals,
							expectedVersion = directPackageVersion(instance),
							_key = key,
						}
					end
				end
				instance = instance.Parent
			end
		end
		table.sort(packages, function(left, right)
			return left._key < right._key
		end)
		for _, package in ipairs(packages) do
			package._key = nil
		end
		return packages
	end

	function api.getMutationPackages(params: { [string]: any }): { [string]: any }
		return { packages = mutationPackages(params.targets, 256) }
	end

	local function validatedTransactionServices(params: { [string]: any }): (string, { string }, { [string]: boolean })
		local transactionId = tostring(params.transactionId or "")
		local servicesAreArray, serviceCount = denseArrayLength(params.services)
		if transactionId == "" or not servicesAreArray or serviceCount < 1 then
			error("Invalid editor transaction")
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
		return transactionId, serviceNames, includedServices
	end

	local function validateTransactionMetadata(
		params: { [string]: any },
		includedServices: { [string]: boolean }
	)
		local metadataServices = validateMutationRequest({
			sourceChanges = params.sourceChanges,
			propertyChanges = params.propertyChanges,
		}, ctx, false)
		for _, serviceName in ipairs(metadataServices) do
			if not includedServices[serviceName] then
				error("Editor transaction metadata is outside its declared services")
			end
		end
	end

	local function validatedPostCommitPropertyChanges(
		rawChanges: any,
		includedServices: { [string]: boolean }
	): { any }
		if rawChanges == nil then
			return {}
		end
		local changesAreArray, changeCount = denseArrayLength(rawChanges)
		if not changesAreArray or changeCount > (tonumber(ctx.maxChangesPerRequest) or 5000) then
			error("Editor post-commit property changes must be a bounded array")
		end
		for changeIndex, change in ipairs(rawChanges) do
			if type(change) ~= "table" then
				error(`Editor post-commit property change {changeIndex} must be an object`)
			end
			local serviceName = change.service
			if type(serviceName) ~= "string" or not includedServices[serviceName] then
				error(`Editor post-commit property change {changeIndex} has an invalid service`)
			end
			validateMutationPath(
				change,
				serviceName,
				`Editor post-commit property change {changeIndex}`,
				ctx
			)
			if change.className ~= "Model" then
				error("Editor post-commit property changes only support Model WorldPivot")
			end
			validateObjectTable(change.properties, "Editor post-commit properties")
			local propertyCount = 0
			for propertyName in pairs(change.properties) do
				propertyCount += 1
				if propertyName ~= "WorldPivot" then
					error("Editor post-commit property changes only support Model WorldPivot")
				end
			end
			if propertyCount ~= 1 or change.properties.WorldPivot == nil then
				error("Editor post-commit property changes require Model WorldPivot")
			end
			if
				change.resetProperties ~= nil
				or change.attributes ~= nil
				or change.deletedAttributes ~= nil
				or change.deleted ~= nil
			then
				error("Editor post-commit property change contains unsupported mutation fields")
			end
		end
		return rawChanges
	end

	local function studioMatchesTransactionExpectation(params: { [string]: any }, serviceNames: { string }): boolean
		local expectedGenerations = params.expectedStudioGenerations
		if expectedGenerations == nil then
			return true
		end
		if type(expectedGenerations) ~= "table" or tostring(params.expectedRuntimeId or "") ~= ctx.runtimeId then
			return false
		end
		for _, serviceName in ipairs(serviceNames) do
			local expected = tonumber(expectedGenerations[serviceName])
			if expected == nil or expected ~= ctx.studioChangeGeneration(serviceName) then
				return false
			end
		end
		return true
	end

	local function validateDestructiveTransactionServices(
		params: { [string]: any },
		includedServices: { [string]: boolean }
	)
		if params.nativeImport == true then
			return
		end
		local destructiveAreArray = denseArrayLength(params.destructiveServices or {})
		if not destructiveAreArray then
			error("Invalid destructive editor transaction services")
		end
		for _, serviceName in ipairs(params.destructiveServices or {}) do
			if
				not includedServices[serviceName]
				or game:GetService(serviceName):FindFirstChildWhichIsA("PackageLink", true) ~= nil
			then
				error(`A destructive {serviceName} change requires the staged Studio importer`)
			end
		end
	end

	local function captureChangedSources(
		changes: { any },
		includedServices: { [string]: boolean }
	): ({ [string]: boolean }, { [Instance]: boolean })
		local changedSourceKeys = {}
		local changedSourceInstances = {}
		for _, change in ipairs(changes) do
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
		return changedSourceKeys, changedSourceInstances
	end

	local function includedNativeImportServices(
		rawServices: any,
		includedServices: { [string]: boolean }
	): { [string]: boolean }
		local nativeImportServices = {}
		for _, serviceName in ipairs(rawServices or {}) do
			if includedServices[serviceName] then
				nativeImportServices[serviceName] = true
			end
		end
		return nativeImportServices
	end

	local function transactionMutationRoots(
		params: { [string]: any },
		includedServices: { [string]: boolean }
	): ({ [string]: { [Instance]: boolean } }, { [string]: boolean }, { [Instance]: boolean })
		local mutationRootsAreArray = denseArrayLength(params.mutationRoots or {})
		if not mutationRootsAreArray then
			error("Invalid editor transaction mutation roots")
		end
		local rootsByService = {}
		local restrictedServices = {}
		local packageSnapshotRoots = {}
		local explicitPackageRoots = params.packageRoots ~= nil
		for index, descriptor in ipairs(params.mutationRoots or {}) do
			local serviceName = tostring(descriptor.service or "")
			if not includedServices[serviceName] then
				error("Invalid editor transaction mutation root service")
			end
			validateMutationPath(descriptor, serviceName, `Editor transaction mutation root {index}`, ctx)
			restrictedServices[serviceName] = true
			local root = resolvePathSegments(descriptor.pathSegments, nil, descriptor.pathOrdinals)
			if root ~= nil and root.Parent == game:GetService(serviceName) then
				local serviceRoots = rootsByService[serviceName]
				if serviceRoots == nil then
					serviceRoots = {}
					rootsByService[serviceName] = serviceRoots
				end
				serviceRoots[root] = true
				if not explicitPackageRoots and containsPackageLink(root) then
					packageSnapshotRoots[root] = true
				end
			end
		end
		if explicitPackageRoots then
			local packageRootsAreArray, packageRootCount = denseArrayLength(params.packageRoots)
			if
				not packageRootsAreArray
				or packageRootCount > (tonumber(ctx.maxChangesPerRequest) or 5000)
			then
				error("Invalid editor transaction package roots")
			end
			for index, descriptor in ipairs(params.packageRoots) do
				if type(descriptor) ~= "table" then
					error(`Editor transaction package root {index} must be an object`)
				end
				local serviceName = tostring(descriptor.pathSegments and descriptor.pathSegments[1] or "")
				if not includedServices[serviceName] then
					error("Invalid editor transaction package root service")
				end
				validateMutationPath(
					descriptor,
					serviceName,
					`Editor transaction package root {index}`,
					ctx
				)
				local root = resolvePathSegments(descriptor.pathSegments, nil, descriptor.pathOrdinals)
				if root == nil or not hasDirectPackageLink(root) then
					error(`Editor transaction package root {index} changed before the transaction began`)
				end
				packageSnapshotRoots[root] = true
			end
		end
		return rootsByService, restrictedServices, packageSnapshotRoots
	end

	local function servicesWithoutNativeImport(
		serviceNames: { string },
		nativeImportServices: { [string]: boolean }
	): { string }
		local snapshotServices = {}
		for _, serviceName in ipairs(serviceNames) do
			if not nativeImportServices[serviceName] then
				snapshotServices[#snapshotServices + 1] = serviceName
			end
		end
		return snapshotServices
	end

	local function changesWithoutNativeImport(
		changes: { any },
		nativeImportServices: { [string]: boolean }
	): { any }
		local filtered = {}
		for _, change in ipairs(changes) do
			if not nativeImportServices[tostring(change.service or "")] then
				filtered[#filtered + 1] = change
			end
		end
		return filtered
	end

	local function captureStudioGenerations(serviceNames: { string }): { [string]: number }
		local generations = {}
		for _, serviceName in ipairs(serviceNames) do
			generations[serviceName] = ctx.studioChangeGeneration(serviceName)
		end
		return generations
	end

	local function studioGenerationsChanged(generations: { [string]: number }): boolean
		for serviceName, generation in pairs(generations) do
			if ctx.studioChangeGeneration(serviceName) ~= generation then
				return true
			end
		end
		return false
	end

	function api.beginTransaction(params: { [string]: any }): { [string]: any }
		pruneExpiredSessions(editorTransactions)
		local operationCancellation = captureOperationCancellation()
		assertSessionOwnership(operationCancellation)
		local transactionId, serviceNames, includedServices = validatedTransactionServices(params)
		if transactionOutcomes.get(transactionId) ~= nil then
			error("Editor transaction id was already completed")
		end
		if editorTransactions[transactionId] ~= nil or countEntries(editorTransactions) > 0 then
			error("Another editor transaction is already active")
		end
		validateTransactionMetadata(params, includedServices)
		local postCommitPropertyChanges =
			validatedPostCommitPropertyChanges(params.postCommitPropertyChanges, includedServices)
		if not studioMatchesTransactionExpectation(params, serviceNames) then
			return { ok = false, studioChanged = true }
		end
		validateDestructiveTransactionServices(params, includedServices)
		local changedSourceKeys, changedSourceInstances =
			captureChangedSources(params.sourceChanges or {}, includedServices)
		local nativeImport = params.nativeImport == true
		local nativeImportServices = includedNativeImportServices(params.nativeImportServices, includedServices)
		local mutationPackageRoots = mutationPackages(
			activeMutationPackageTargets(params),
			tonumber(ctx.maxChangesPerRequest) or 5000
		)
		local transactionParams = table.clone(params)
		transactionParams.packageRoots = table.clone(params.packageRoots or {})
		for _, packageRoot in ipairs(mutationPackageRoots) do
			transactionParams.packageRoots[#transactionParams.packageRoots + 1] = packageRoot
		end
		local mutationRootsByService, restrictedMutationServices, packageSnapshotRoots =
			transactionMutationRoots(transactionParams, includedServices)
		local snapshotServices = servicesWithoutNativeImport(serviceNames, nativeImportServices)
		local snapshotSourceChanges = changesWithoutNativeImport(params.sourceChanges or {}, nativeImportServices)
		local snapshotPropertyChanges = changesWithoutNativeImport(params.propertyChanges or {}, nativeImportServices)
		local studioGenerations = captureStudioGenerations(serviceNames)
		local snapshot = TransactionState.captureSnapshot(snapshotServices, {
			forceStructural = not nativeImport and params.hasInstanceChanges == true or next(packageSnapshotRoots) ~= nil,
			captureAllScriptDocuments = nativeImport,
			instanceChanges = {},
			sourceChanges = snapshotSourceChanges,
			propertyChanges = snapshotPropertyChanges,
			packageSnapshotRoots = packageSnapshotRoots,
			mutationRootsByService = mutationRootsByService,
			restrictedMutationServices = restrictedMutationServices,
		}, ctx)
		assertSessionOwnership(operationCancellation)
		if
			not studioMatchesTransactionExpectation(params, serviceNames)
			or studioGenerationsChanged(studioGenerations)
		then
			return { ok = false, studioChanged = true }
		end
		local session = {
			transactionId = transactionId,
			leaseId = currentRequestLeaseId(),
			state = "open",
			serviceNames = serviceNames,
			changedSourceKeys = changedSourceKeys,
			changedSourceInstances = changedSourceInstances,
			instanceReplacements = {},
			snapshot = snapshot,
			mutated = false,
			nativeImport = nativeImport,
			nativeImportServices = nativeImportServices,
			packageSnapshotRoots = packageSnapshotRoots,
			postCommitPropertyChanges = postCommitPropertyChanges,
			studioGenerations = studioGenerations,
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
				return TransactionState.rollbackSession(session, ctx)
			end)
			if not ok then
				error(result, 0)
			end
			recordTransactionOutcome(transactionId, "rolledBack", {
				replacements = countEntries(result),
			})
		end
		editorTransactions[transactionId] = session
		armSessionExpiry(editorTransactions, transactionId, session)
		return {
			ok = true,
			found = true,
			transactionId = transactionId,
			state = "open",
			packageMutation = next(packageSnapshotRoots) ~= nil,
			mutationPackages = mutationPackageRoots,
		}
	end

	local function commitTransactionUnsafe(params: { [string]: any }, operation): { [string]: any }
		pruneExpiredSessions(editorTransactions)
		local profile = params.profile == true
		local timings = {}
		local phaseStarted = os.clock()
		local transactionId = tostring(params.transactionId or "")
		local session = editorTransactions[transactionId]
		if type(session) ~= "table" then
			local outcome = transactionOutcomes.get(transactionId)
			if outcome ~= nil then
				return outcome
			end
			error("Editor transaction was not found")
		end
		assertTransactionLease(session)
		local operationCancellation = captureOperationCancellation()
		local function assertCommitActive()
			assertSessionOwnership(operationCancellation)
			if session.cancelRequested then
				error("Renium request lease was cancelled")
			end
		end
		assertCommitActive()
		for serviceName, generation in pairs(session.studioGenerations or {}) do
			local currentGeneration = ctx.studioChangeGeneration(serviceName)
			if currentGeneration ~= generation then
				local records = TransactionState.drainJournal(session, ctx)
				local record = records[1]
				local detail = "no tracked event"
				if record ~= nil then
					local kinds = {}
					for propertyName in pairs(record.properties or {}) do
						kinds[#kinds + 1] = "property " .. propertyName
					end
					for attributeName in pairs(record.attributes or {}) do
						kinds[#kinds + 1] = "attribute " .. attributeName
					end
					if record.structural then
						kinds[#kinds + 1] = "structure"
					end
					if record.tagsChanged then
						kinds[#kinds + 1] = "tags"
					end
					detail = `{table.concat(record.pathSegments or {}, ".")} ({table.concat(kinds, ", ")})`
				end
				error(
					`Studio changed {serviceName} while the filesystem transaction was staged ({generation} -> {currentGeneration}; {detail}); retry the sync`
				)
			end
		end
		if session.nativeUndo ~= nil then
			setTrackedOperationPhase(operation, "nativeCommit")
			ReferenceOverlay.chainReplacements(session.instanceReplacements, session.nativeUndo.replacements)
			TransactionState.drainJournal(session, ctx)
			-- Native commit can replace live roots before reporting an error.
			session.mutated = true
			ReferenceOverlay.commitNative(session.nativeUndo, ctx)
			assertCommitActive()
			for original, replacement in pairs(session.nativeUndo.replacements) do
				session.instanceReplacements[original] = replacement
			end
		end
		timings.nativeCommitMs = (os.clock() - phaseStarted) * 1000
		phaseStarted = os.clock()
		setTrackedOperationPhase(operation, "scriptDocuments")
		ScriptDocumentState.apply(
			session.snapshot.scriptDocuments or {},
			session.changedSourceInstances,
			session.changedSourceKeys,
			session.instanceReplacements,
			session.resolveStagedPath,
			session.nativeImport
		)
		assertCommitActive()
		timings.scriptDocumentsMs = (os.clock() - phaseStarted) * 1000
		phaseStarted = os.clock()
		local postCommitPropertyChanges = session.postCommitPropertyChanges
		if #postCommitPropertyChanges > 0 then
			local updated = 0
			RunService.Heartbeat:Wait()
			assertCommitActive()
			for _, change in ipairs(postCommitPropertyChanges) do
				assertCommitActive()
				local _, service = validatedChangeService(change, ctx)
				local instance = resolveInstance(change, ctx)
				if instance == nil or not instance:IsA("Model") then
					error(`WorldPivot target was not found: {pathKey(change.pathSegments)}`)
				end
				assertInstanceInService(instance, service)
				local okDecode, value = decodePropertyValue(
					instance,
					"WorldPivot",
					change.properties.WorldPivot,
					ctx,
					change.service
				)
				if not okDecode then
					error(`Failed to decode WorldPivot: {value}`)
				end
				local okRead, current = readProperty(instance, "WorldPivot")
				if not okRead or current ~= value then
					local okWrite, writeError = writePropertyForSync(instance, "WorldPivot", value, ctx)
					if not okWrite then
						error(
							`Failed to write WorldPivot for {pathKey(change.pathSegments)} on {instance:GetFullName()}: {writeError}`
						)
					end
					updated += 1
				end
			end
			timings.postCommitPropertyUpdated = updated
		end
		timings.postCommitPropertiesMs = (os.clock() - phaseStarted) * 1000
		phaseStarted = os.clock()
		setTrackedOperationPhase(operation, "journal")
		local records = TransactionState.finishJournal(session, ctx)
		TransactionState.replayJournal(records, session.instanceReplacements, ctx)
		assertCommitActive()
		timings.journalMs = (os.clock() - phaseStarted) * 1000
		phaseStarted = os.clock()
		setTrackedOperationPhase(operation, "history")
		assertCommitActive()
		finishHistoryRecording(session.historyRecording)
		session.commitFence = true
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
		setTrackedOperationPhase(operation, "cleanup")
		local nativeUndo = session.nativeUndo
		session.nativeUndo = nil
		session.resolveStagedPath = nil
		if nativeUndo ~= nil then
			local cleanupOk, cleanupError = pcall(function()
				for _, merge in ipairs(nativeUndo.packageMerges or {}) do
					for _, instance in ipairs(merge.outgoing) do
						if instance.Parent == nil then
							instance:Destroy()
						end
					end
				end
				for _, group in ipairs(nativeUndo.prepared) do
					for _, instance in ipairs(group.outgoing) do
						if instance.Parent == nil then
							instance:Destroy()
						end
					end
				end
				for _, instance in ipairs(nativeUndo.retainedDuplicates or {}) do
					instance:Destroy()
				end
			end)
			if not cleanupOk then
				warn("[Renium] committed transaction cleanup failed: " .. tostring(cleanupError))
			end
		end
		timings.cleanupMs = (os.clock() - phaseStarted) * 1000
		for _, serviceName in ipairs(session.serviceNames) do
			invalidateEditorService(serviceName)
		end
		timings.invalidatedServices = #session.serviceNames
		editorTransactions[transactionId] = nil
		return recordTransactionOutcome(transactionId, "committed", {
			undoRecorded = undoRecorded,
			profile = if profile then timings else nil,
		})
	end

	function api.commitTransaction(params: { [string]: any }): { [string]: any }
		local transactionId = tostring(params.transactionId or "")
		local activeSession = editorTransactions[transactionId]
		local operation = beginTrackedOperation("editorCommit")
		if type(activeSession) == "table" then
			beginSessionOperation(activeSession)
		end
		local ok, result = xpcall(commitTransactionUnsafe, debug.traceback, params, operation)
		if type(activeSession) == "table" then
			endSessionOperation(editorTransactions, transactionId, activeSession)
		end
		finishTrackedOperation(operation)
		if ok then
			return result
		end
		local session = editorTransactions[transactionId]
		if type(session) == "table" then
			if session.commitFence then
				session.onExpire = nil
				editorTransactions[transactionId] = nil
				return recordTransactionOutcome(transactionId, "committed", {
					commitWarning = tostring(result),
				})
			end
			local rollbackOk, rollbackResult = pcall(runWithStudioChangeSuppression, ctx, function()
				return TransactionState.rollbackSession(session, ctx)
			end)
			if rollbackOk then
				session.onExpire = nil
				editorTransactions[transactionId] = nil
				recordTransactionOutcome(transactionId, "rolledBack", {
					replacements = countEntries(rollbackResult),
				})
				for _, serviceName in ipairs(session.serviceNames) do
					invalidateEditorService(serviceName)
				end
			else
				armSessionExpiry(editorTransactions, transactionId, session)
				error(`{result}\nEditor rollback also failed: {rollbackResult}`, 0)
			end
		end
		error(result, 0)
	end

	function api.rollbackTransaction(params: { [string]: any }): { [string]: any }
		local transactionId = tostring(params.transactionId or "")
		local session = editorTransactions[transactionId]
		if type(session) ~= "table" then
			local outcome = transactionOutcomes.get(transactionId)
			if outcome ~= nil then
				return outcome
			end
			return {
				ok = true,
				found = false,
				transactionId = transactionId,
				state = "notFound",
				committed = false,
				rolledBack = false,
			}
		end
		assertTransactionLease(session)
		local operation = beginTrackedOperation("editorRollback")
		beginSessionOperation(session)
		local ok, replacements = pcall(runWithStudioChangeSuppression, ctx, function()
			return TransactionState.rollbackSession(session, ctx)
		end)
		endSessionOperation(editorTransactions, transactionId, session)
		finishTrackedOperation(operation)
		if not ok then
			armSessionExpiry(editorTransactions, transactionId, session)
			error(replacements, 0)
		end
		session.onExpire = nil
		editorTransactions[transactionId] = nil
		for _, serviceName in ipairs(session.serviceNames) do
			invalidateEditorService(serviceName)
		end
		return recordTransactionOutcome(transactionId, "rolledBack", {
			replacements = countEntries(replacements),
		})
	end

	function api.beginBinaryExport(params: { [string]: any }): { [string]: any }
		local profile = params.profile == true
		local profileStarted = if profile then os.clock() else 0
		local profileTimings = {}
		local phaseStarted = profileStarted
		pruneExpiredSessions(binaryExports)
		local operationCancellation = captureOperationCancellation()
		assertSessionOwnership(operationCancellation)
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
		if params.serviceFilter ~= nil then
			local filterIsArray, filterCount = denseArrayLength(params.serviceFilter)
			if not filterIsArray or filterCount < 1 then
				error("Invalid native export service filter")
			end
			local allowedByName = {}
			for _, serviceName in ipairs(serviceNames) do
				allowedByName[serviceName] = true
			end
			local filteredNames = table.create(filterCount)
			local included = {}
			for index, rawServiceName in ipairs(params.serviceFilter) do
				local serviceName = tostring(rawServiceName)
				if not allowedByName[serviceName] or included[serviceName] then
					error("Invalid native export service filter")
				end
				included[serviceName] = true
				filteredNames[index] = serviceName
			end
			serviceNames = filteredNames
		end
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
		for _, serviceName in ipairs(serviceNames) do
			assertSessionOwnership(operationCancellation)
			local service = game:GetService(serviceName)
			local marker = NativeSerialization.snapshotMarker(serviceName, service)
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
			}
			groups[#groups + 1] = group
			groupByService[serviceName] = group
		end
		if profile then
			profileTimings.layoutMs = (os.clock() - phaseStarted) * 1000
			phaseStarted = os.clock()
		end
		local session: { [string]: any } = {
			leaseId = currentRequestLeaseId(),
			groups = groups,
			serviceNames = serviceNames,
			nativeStates = {},
			partitioned = partitioned,
			status = "pending",
			structureChanged = false,
			payloads = {},
			payloadHashes = {},
			payloadSizes = {},
			payloadErrors = {},
			cachedPayloadProofs = {},
			binaryBatchPayloads = {},
			payloadReadyEvent = Instance.new("BindableEvent"),
			serializationJobs = {},
			serializationBatches = {},
			serializationBatchIds = {},
			originalNonArchivableInstances = {},
			snapshotRoots = table.clone(markers),
			activeSerializations = 0,
			nonArchivableByService = {},
			serializationScheduleReady = not partitioned,
			pendingPayloads = if partitioned then 0 else 1,
			updatedAt = os.clock(),
			ctx = ctx,
		}
		if not metadataOnly then
			local guardedServices = table.create(#serviceNames)
			for index, serviceName in ipairs(serviceNames) do
				guardedServices[index] = {
					serviceName = serviceName,
					service = game:GetService(serviceName),
				}
			end
			session.nativeGuard = ReferenceOverlay.beginNativeGuard(guardedServices, true, {
				archivable = true,
			})
		end
		if profile then
			profileTimings.nativeGuardMs = (os.clock() - phaseStarted) * 1000
		end
		session.cleanupSnapshot = function()
			if session.activeSerializations > 0 then
				session.snapshotCleanupPending = true
				return
			end
			session.snapshotCleanupPending = false
			table.clear(session.snapshotRoots)
		end
		session.onExpire = function()
			if session.released then
				return
			end
			session.cancelled = true
			session.released = true
			session.serializationScheduleReady = true
			session.payloadReadyEvent:Fire()
			ReferenceOverlay.finishNativeGuard(session.nativeGuard)
			session.nativeGuard = nil
			session.cleanupSnapshot()
			table.clear(session.nativeStates)
			table.clear(session.binaryBatchPayloads)
			session.payloadReadyEvent:Destroy()
		end
		binaryExports[exportId] = session
		armSessionExpiry(binaryExports, exportId, session)
		local beginOk, beginResult = xpcall(function()
			if not metadataOnly then
				phaseStarted = if profile then os.clock() else 0
				local scriptSourcesByInstance = {}
				for _, entry in ipairs(ScriptDocumentState.capture(serviceNames)) do
					scriptSourcesByInstance[entry.instance] = entry.source
				end
				if profile then
					profileTimings.scriptDocumentsMs = (os.clock() - phaseStarted) * 1000
				end
				assertSessionOwnership(operationCancellation)
				local serializerWorkerCount = if partitioned
					then math.clamp(math.floor(tonumber(params.serializationWorkers) or #groups), 1, #groups)
					else 0
				if partitioned then
					session.firstUnscheduledSerializationGroup = serializerWorkerCount + 1
					session.serializerWorkerCount = serializerWorkerCount
					NativeSerialization.startWorkers(session, serializerWorkerCount)
				end
				local prepareNativeStateMs = 0
				local identityCarriersMs = 0
				local rootPropertiesMs = 0
				local nativePreparationByService = {}
				for serviceIndex, serviceName in ipairs(serviceNames) do
					assertSessionOwnership(operationCancellation)
					phaseStarted = if profile then os.clock() else 0
					local state = ctx.prepareNativeState(serviceName, scriptSourcesByInstance)
					if profile then
						prepareNativeStateMs += (os.clock() - phaseStarted) * 1000
						nativePreparationByService[serviceName] = state.nativePreparationProfile
						phaseStarted = os.clock()
					end
					session.nativeStates[serviceName] = state
					session.nonArchivableByService[serviceName] = state.nonArchivableInstances
					local group = groupByService[serviceName]
					NativeSerialization.appendIdentityCarriers(
						group,
						rootsByService[serviceName],
						state.instances,
						state.nativeStructureGeneration
					)
					if profile then
						identityCarriersMs += (os.clock() - phaseStarted) * 1000
						phaseStarted = os.clock()
					end
					local values = ctx.readRootProperties(serviceName, state)
					if type(values) == "table" and next(values) then
						group.rootProperties = values
					end
					if profile then
						rootPropertiesMs += (os.clock() - phaseStarted) * 1000
					end
					if partitioned and serviceIndex <= serializerWorkerCount then
						NativeSerialization.appendJob(
							session,
							serviceName,
							{ serviceName },
							rootsByService[serviceName],
							NativeSerialization.nonArchivableInstances(session, { serviceName })
						)
					end
				end
				if profile then
					profileTimings.prepareNativeStateMs = prepareNativeStateMs
					profileTimings.nativePreparationByService = nativePreparationByService
					profileTimings.identityCarriersMs = identityCarriersMs
					profileTimings.rootPropertiesMs = rootPropertiesMs
					phaseStarted = os.clock()
				end
				ReferenceOverlay.assertNativeGuard(session.nativeGuard)
				if profile then
					profileTimings.referenceGuardMs = (os.clock() - phaseStarted) * 1000
				end
				if not partitioned then
					table.clear(roots)
					for _, serviceName in ipairs(serviceNames) do
						for _, root in ipairs(rootsByService[serviceName]) do
							roots[#roots + 1] = root
						end
					end
					local job = {
						roots = roots,
						nonArchivableInstances = NativeSerialization.nonArchivableInstances(session, serviceNames),
					}
					for _, instance in ipairs(job.nonArchivableInstances) do
						session.originalNonArchivableInstances[instance] = true
					end
					task.spawn(NativeSerialization.runJob, session, job)
				end
			else
				session.pendingPayloads = 0
				session.serializationScheduleReady = true
				session.status = "ready"
			end
			phaseStarted = if profile then os.clock() else 0
			local propertySchemaByClass = {}
			local enumValueNamesByType = {}
			for _, serviceName in ipairs(serviceNames) do
				assertSessionOwnership(operationCancellation)
				local state = session.nativeStates[serviceName]
				if state == nil then
					state = ctx.getState(serviceName)
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
				group.scriptCount = #(state.scriptObjects or {})
				group.classNames = state.classNames or {}
				group.nonArchivableIndices = state.nativeNonArchivableIndices or {}
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
				NativeSerialization.finishSchedule(
					session,
					groups,
					rootsByService,
					session.firstUnscheduledSerializationGroup
				)
			end
			if profile then
				profileTimings.schemaMs = (os.clock() - phaseStarted) * 1000
				profileTimings.totalMs = (os.clock() - profileStarted) * 1000
			end
			return {
				ok = true,
				exportId = exportId,
				groups = groups,
				serializationBatches = session.serializationBatches,
				propertySchemaByClass = propertySchemaByClass,
				enumValueNamesByType = enumValueNamesByType,
				pending = session.status == "pending",
				profile = if profile then profileTimings else nil,
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
		local requestLeaseId = currentRequestLeaseId()
		if session.leaseId ~= nil and requestLeaseId ~= nil and session.leaseId ~= requestLeaseId then
			error("Native export belongs to another request lease")
		end
		assertRequestLeaseActive(session.leaseId)
		local state = session.nativeStates and session.nativeStates[serviceName]
		if state == nil then
			error("Native export service state was not found")
		end
		ReferenceOverlay.assertNativeGuard(session.nativeGuard)
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
		local requestLeaseId = currentRequestLeaseId()
		if session.leaseId ~= nil and requestLeaseId ~= nil and session.leaseId ~= requestLeaseId then
			error("Native export belongs to another request lease")
		end
		assertRequestLeaseActive(session.leaseId)
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
					and session.cachedPayloadProofs[serviceName] == nil
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
						and session.cachedPayloadProofs[serviceName] == nil
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
					totalBytes = if payload then buffer.len(payload) else session.payloadSizes[serviceName],
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
			local cached = if session.partitioned then session.cachedPayloadProofs[serviceName] else nil
			if cached ~= nil then
				if not NativeSerialization.proofMatches(session, cached.proof) then
					nativePayloadProofs[serviceName] = nil
					error("Studio changed during native export")
				end
				if
					params.rawBase64 == true
					and params.supportsPayloadCache == true
					and cached.proof.payloadHash == tostring(params.knownPayloadHash or "")
				then
					return {
						start = 1,
						nextStart = 1,
						total = cached.proof.totalBytes,
						chunk = "",
						pluginEncodeMs = 0,
						serializationComplete = session.status == "ready",
						payloadHash = cached.proof.payloadHash,
						payloadCacheHit = true,
					}
				end
				NativeSerialization.materializeCachedPayload(session, serviceName)
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
			local payloadHash = if session.partitioned then session.payloadHashes[serviceName] else session.payloadHash
			if
				params.rawBase64 == true
				and params.supportsPayloadCache == true
				and offset == 0
				and payloadHash ~= nil
				and payloadHash == tostring(params.knownPayloadHash or "")
			then
				return encodeBinaryChunk(
					payload,
					0,
					totalBytes,
					totalBytes,
					session.status == "ready",
					payloadHash,
					payloadHash,
					true
				)
			end
			local chunk = if offset == 0 and length == totalBytes then payload else buffer.create(length)
			if chunk ~= payload then
				buffer.copy(chunk, 0, payload, offset, length)
			end
			if params.rawBase64 == true then
				return encodeBinaryChunk(
					chunk,
					offset,
					length,
					totalBytes,
					session.status == "ready",
					payloadHash,
					tostring(params.knownPayloadHash or ""),
					params.supportsPayloadCache == true
				)
			end
			return {
				ok = true,
				offset = offset,
				length = length,
				data = chunk,
			}
		end)
	end

	local function validatedBinaryExportBatchServices(params: { [string]: any }, session: { [string]: any }): { string }
		local denseServices, serviceCount = denseArrayLength(params.services)
		if not denseServices or serviceCount < 2 or serviceCount > #session.serviceNames then
			error("Invalid native export batch services")
		end
		local serviceNames = table.create(serviceCount)
		local included = {}
		for index, rawServiceName in ipairs(params.services) do
			local serviceName = tostring(rawServiceName)
			if serviceName == "" or included[serviceName] or session.nativeStates[serviceName] == nil then
				error("Invalid native export batch service")
			end
			included[serviceName] = true
			serviceNames[index] = serviceName
		end
		return serviceNames
	end

	local function binaryExportBatchPending(session: { [string]: any }, serviceNames: { string }): boolean
		for _, serviceName in ipairs(serviceNames) do
			if session.payloads[serviceName] == nil and session.payloadErrors[serviceName] == nil then
				return true
			end
		end
		return false
	end

	local function awaitBinaryExportBatchPayloads(
		exportId: string,
		session: { [string]: any },
		serviceNames: { string },
		timeoutSeconds: number
	)
		if session.structureChanged then
			error("Studio structure changed during native export")
		end
		local deadline = os.clock() + timeoutSeconds
		local timeoutThread = task.delay(timeoutSeconds, function()
			if binaryExports[exportId] == session then
				session.payloadReadyEvent:Fire()
			end
		end)
		while not session.cancelled and os.clock() < deadline and binaryExportBatchPending(session, serviceNames) do
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
	end

	local function buildBinaryExportBatch(session: { [string]: any }, serviceNames: { string }): { [string]: any }
		local lengths = table.create(#serviceNames)
		local batchedServices = table.create(#serviceNames)
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
		for index, payloadLength in ipairs(lengths) do
			buffer.writeu32(header, 4 + (index - 1) * 4, payloadLength)
		end
		return {
			header = header,
			lengths = lengths,
			services = batchedServices,
			totalBytes = buffer.len(header) + payloadBytes,
		}
	end

	local function binaryExportBatch(
		exportId: string,
		session: { [string]: any },
		serviceNames: { string },
		timeoutSeconds: number
	): { [string]: any }
		for _, serviceName in ipairs(serviceNames) do
			NativeSerialization.materializeCachedPayload(session, serviceName)
		end
		local cacheKey = table.concat(serviceNames, PATH_SEPARATOR)
		local batch = session.binaryBatchPayloads[cacheKey]
		if batch ~= nil then
			return batch
		end
		awaitBinaryExportBatchPayloads(exportId, session, serviceNames, timeoutSeconds)
		batch = session.binaryBatchPayloads[cacheKey]
		if batch == nil then
			batch = buildBinaryExportBatch(session, serviceNames)
			session.binaryBatchPayloads[cacheKey] = batch
		end
		return batch
	end

	local function readBinaryExportBatchChunk(
		session: { [string]: any },
		batch: { [string]: any },
		offset: number,
		length: number
	): buffer
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
			for index, serviceName in ipairs(batch.services) do
				local payloadLength = batch.lengths[index]
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
		return chunk
	end

	function api.readBinaryExportBatch(params: { [string]: any }): { [string]: any }
		local exportId, _, session = binaryExportSession(params, true)
		return runSessionOperation(binaryExports, exportId, session, function()
			local serviceNames = validatedBinaryExportBatchServices(params, session)
			local timeoutSeconds = math.clamp(tonumber(params.timeoutSeconds) or 30, 1, 120)
			local batch = binaryExportBatch(exportId, session, serviceNames, timeoutSeconds)
			local totalBytes = batch.totalBytes
			local offset, length = binaryReadRange(params, totalBytes, "Native export batch")
			local chunk = readBinaryExportBatchChunk(session, batch, offset, length)
			session.updatedAt = os.clock()
			return encodeBinaryChunk(chunk, offset, length, totalBytes, session.status == "ready")
		end)
	end

	function api.finishBinaryExport(params: { [string]: any }): { [string]: any }
		local exportId = tostring(params.exportId or "")
		local session = binaryExports[exportId]
		local found = session ~= nil
		if type(session) == "table" then
			if session.leaseId ~= nil and session.leaseId ~= currentRequestLeaseId() then
				error("Native export belongs to another request lease")
			end
			local changedService = if session.nativeGuard then session.nativeGuard.changedService else nil
			session.cancelled = true
			session.payloadReadyEvent:Fire()
			expireSession(binaryExports, exportId, session)
			if changedService ~= nil then
				NativeSerialization.invalidateProofs(changedService)
				error(`Studio changed {changedService} during native export; retry the sync`)
			end
		end
		return { ok = true, found = found }
	end

	local function validatedBinaryImportSize(params: { [string]: any }): (string, number, number)
		local importId = tostring(params.importId or "")
		local totalBytes = tonumber(params.totalBytes)
		if importId == "" or not totalBytes or totalBytes < 1 or totalBytes > 536870912 or totalBytes % 1 ~= 0 then
			error("Invalid native import size")
		end
		local totalChunks = tonumber(params.totalChunks)
		if
			not totalChunks
			or totalChunks ~= math.ceil(totalBytes / BINARY_IMPORT_CHUNK_BYTES)
			or totalChunks < 1
			or totalChunks > 4096
			or totalChunks % 1 ~= 0
		then
			error("Invalid native import chunk count")
		end
		return importId, totalBytes, totalChunks
	end

	local function validatedBinaryImportInstanceCount(params: { [string]: any }): number
		local instanceCount = tonumber(params.instanceCount)
		if not instanceCount or instanceCount ~= instanceCount or instanceCount < 0 or instanceCount % 1 ~= 0 then
			error("Invalid native import instance count")
		end
		return instanceCount
	end

	local function assertBinaryImportCapacity(importId: string, totalBytes: number)
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
	end

	local function validatedNativeImportTarget(
		rawGroup: { [string]: any }
	): (string, Instance, Instance, { string }, number)
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
		return serviceName, service, target, targetPath, targetPathLength
	end

	local function validateNativeImportChildPath(
		descriptor: { [string]: any },
		targetPath: { string },
		targetPathLength: number,
		lengthError: string,
		outsideError: string
	)
		if #descriptor.pathSegments ~= targetPathLength + 1 or #descriptor.pathOrdinals ~= #descriptor.pathSegments then
			error(lengthError)
		end
		for index = 1, targetPathLength do
			if descriptor.pathSegments[index] ~= targetPath[index] then
				error(outsideError)
			end
		end
	end

	local function validatedNativeImportRootPaths(
		rawGroup: { [string]: any },
		serviceName: string,
		targetPath: { string },
		targetPathLength: number,
		count: number
	): { any }
		local rootPathsAreArray, rootPathCount = denseArrayLength(rawGroup.rootPaths)
		if not rootPathsAreArray or rootPathCount ~= count then
			error("Invalid native import root paths")
		end
		local rootPaths = table.create(count)
		local rootPathKeys = {}
		for rootIndex, descriptor in ipairs(rawGroup.rootPaths) do
			validateObjectTable(descriptor, "Native import root path")
			validateMutationPath(descriptor, serviceName, `Native import root path {rootIndex}`, ctx)
			validateNativeImportChildPath(
				descriptor,
				targetPath,
				targetPathLength,
				"Native import root path has an invalid length",
				"Native import root path is outside its target"
			)
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
		return rootPaths
	end

	local function validatedNativeImportRetainedRoots(
		rawGroup: { [string]: any },
		serviceName: string,
		targetPath: { string },
		targetPathLength: number,
		count: number
	): { any }
		if rawGroup.retainedRoots == nil then
			return {}
		end
		local retainedAreArray, retainedCount = denseArrayLength(rawGroup.retainedRoots)
		if not retainedAreArray or retainedCount > count then
			error("Invalid native import retained roots")
		end
		local retainedRoots = table.create(retainedCount)
		local retainedPayloadIndexes = {}
		for retainedIndex, descriptor in ipairs(rawGroup.retainedRoots) do
			validateObjectTable(descriptor, "Native import retained root")
			validateMutationPath(descriptor, serviceName, `Native import retained root {retainedIndex}`, ctx)
			validateNativeImportChildPath(
				descriptor,
				targetPath,
				targetPathLength,
				"Native import retained root has an invalid path",
				"Native import retained root is outside its target"
			)
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
			retainedRoots[retainedIndex] = {
				pathSegments = table.clone(descriptor.pathSegments),
				pathOrdinals = table.clone(descriptor.pathOrdinals),
				className = className,
				payloadIndex = payloadIndex,
				instanceCount = retainedInstanceCount,
				payloadOmitted = descriptor.payloadOmitted == true,
			}
		end
		return retainedRoots
	end

	local function validatedNativeImportGroup(
		rawGroup: { [string]: any },
		payloadRootNames: { [string]: boolean }
	): { [string]: any }
		validateObjectTable(rawGroup, "Native import group")
		local serviceName, service, target, targetPath, targetPathLength = validatedNativeImportTarget(rawGroup)
		local count = tonumber(rawGroup.count)
		if not count or count < 0 or count % 1 ~= 0 then
			error("Invalid native import service count")
		end
		local rootPaths = validatedNativeImportRootPaths(rawGroup, serviceName, targetPath, targetPathLength, count)
		local payloadRootName = rawGroup.payloadRootName
		if type(payloadRootName) ~= "string" or payloadRootName == "" then
			error("Invalid native import payload root")
		end
		if payloadRootNames[payloadRootName] then
			error("Duplicate native import payload root")
		end
		payloadRootNames[payloadRootName] = true
		local retainedRoots =
			validatedNativeImportRetainedRoots(rawGroup, serviceName, targetPath, targetPathLength, count)
		local packageRoots = validatedPackageDescriptors(
			rawGroup.packageRoots,
			"native import package root",
			serviceName,
			targetPath,
			count,
			ctx
		)
		local changeGeneration = tonumber(rawGroup.changeGeneration)
		if
			(#retainedRoots > 0 or #packageRoots > 0)
			and (not changeGeneration or changeGeneration < 0 or changeGeneration % 1 ~= 0)
		then
			error("Native import retained roots require a Studio change generation")
		end
		return {
			serviceName = serviceName,
			service = service,
			target = target,
			targetPath = table.clone(targetPath),
			count = count,
			payloadRootName = payloadRootName,
			rootPaths = rootPaths,
			retainedRoots = retainedRoots,
			packageRoots = packageRoots,
			changeGeneration = changeGeneration,
		}
	end

	local function validatedNativeImportGroups(rawGroups: any): { any }
		local groupsAreArray, groupCount = denseArrayLength(rawGroups)
		if not groupsAreArray or groupCount == 0 then
			error("Native import groups must be an array")
		end
		local groups = table.create(groupCount)
		local payloadRootNames = {}
		for index, rawGroup in ipairs(rawGroups) do
			groups[index] = validatedNativeImportGroup(rawGroup, payloadRootNames)
		end
		return groups
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
		assertTransactionLease(transaction)
		armSessionExpiry(editorTransactions, transactionId, transaction)
		local totalBytes, totalChunks
		importId, totalBytes, totalChunks = validatedBinaryImportSize(params)
		if type(params.externalReferencesPostApplied) ~= "boolean" then
			error("Native import external reference policy is invalid")
		end
		local instanceCount = validatedBinaryImportInstanceCount(params)
		assertBinaryImportCapacity(importId, totalBytes)
		local groups = validatedNativeImportGroups(params.groups)
		binaryImports[importId] = {
			leaseId = transaction.leaseId,
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
		assertTransactionLease(transaction)
		assertRequestLeaseActive(session.leaseId)
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
			if completed.leaseId ~= nil and completed.leaseId ~= currentRequestLeaseId() then
				error("Native import belongs to another request lease")
			end
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
		assertTransactionLease(transaction)
		assertRequestLeaseActive(session.leaseId)
		local operationGeneration = captureOperationCancellation()
		assertSessionOwnership(operationGeneration)
		armSessionExpiry(binaryImports, importId, session)
		armSessionExpiry(editorTransactions, transactionId, transaction)
		beginSessionOperation(session)
		beginSessionOperation(transaction)
		local operation = beginTrackedOperation("binaryImport")
		local roots = {}
		local detachedRoots = {}
		local okFinish, responseOrError = xpcall(function()
			local function assertImportActive()
				assertSessionOwnership(operationGeneration)
				if session.cancelRequested then
					error("Native import was cancelled")
				end
			end
			local started = os.clock()
			setTrackedOperationPhase(operation, "scanOutgoing")
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
			setTrackedOperationPhase(operation, "deserialize")
			roots = SerializationService:DeserializeInstancesAsync(session.payload)
			setTrackedOperationPhase(operation, "validatePayload")
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
			setTrackedOperationPhase(operation, "prepareGroups")
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
				for _, instance in ipairs(groupRoots) do
					local prefix = "__ReniumImportRoot_"
					local payloadIndex = if string.sub(instance.Name, 1, #prefix) == prefix
						then tonumber(string.sub(instance.Name, #prefix + 1))
						else nil
					if
						not payloadIndex
						or payloadIndex < 1
						or payloadIndex > group.count
						or payloadIndex % 1 ~= 0
						or incomingByPayloadIndex[payloadIndex] ~= nil
					then
						error("Native import returned an invalid payload index")
					end
					incomingByPayloadIndex[payloadIndex] = instance
				end
				for index = 1, group.count do
					local instance = incomingByPayloadIndex[index]
					if instance == nil or instance.Parent ~= groupPayloadRoot or instance:IsA("Terrain") then
						error("Native import returned an invalid root")
					end
					local descriptor = group.rootPaths[index]
					local pathSegments = descriptor and descriptor.pathSegments
					local originalName = if type(pathSegments) == "table" then pathSegments[#pathSegments] else nil
					if type(originalName) ~= "string" then
						error("Native import root path is invalid")
					end
					instance.Name = originalName
					setParentForSync(instance, nil, ctx)
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
			setTrackedOperationPhase(operation, "prepareRetention")
			local retention = ReferenceOverlay.prepareRetained(prepared, ctx, session.externalReferencesPostApplied)
			transaction.nativeUndo = {
				prepared = prepared,
				replacements = retention.replacements,
				currentCamera = previousCamera,
				resolveStagedPath = retention.resolveStagedPath,
				referenceUpdates = retention.referenceUpdates,
				retainedDuplicates = retention.retainedDuplicates,
				packageAliases = retention.packageAliases,
				packageMerges = retention.packageMerges,
				packageStatePairs = retention.packageStatePairs,
				generationsByService = generationsByService,
				guard = transaction.nativeGuard,
			}
			transaction.nativeGuard = nil
			transaction.resolveStagedPath = retention.resolveStagedPath
			assertImportActive()
			setTrackedOperationPhase(operation, "applyOverlay")
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
			transaction.state = "prepared"
			binaryImports[importId] = nil
			completedBinaryImports[importId] = {
				leaseId = session.leaseId,
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
		finishTrackedOperation(operation)
		if not okFinish then
			local rollbackError = nil
			if transaction.nativeUndo ~= nil then
				local undo = transaction.nativeUndo
				local rolledBack, result = pcall(ReferenceOverlay.rollbackNative, undo, ctx)
				if rolledBack then
					transaction.nativeUndo = nil
				else
					rollbackError = tostring(result)
				end
			end
			ReferenceOverlay.finishNativeGuard(transaction.nativeGuard)
			transaction.nativeGuard = nil
			if rollbackError == nil then
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
			else
				error(`{responseOrError}\nNative import rollback failed: {rollbackError}`, 0)
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
		local operationGeneration = captureOperationCancellation()
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
			assertTransactionLease(outerTransaction)
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
					transactionSnapshot = TransactionState.captureSnapshot(serviceNames, params, ctx)
				end
			else
				local session = reconcileSessions[chunkSessionKey]
				if type(session) ~= "table" or session.transactionId ~= requestedTransactionId then
					error("Editor reconcile session was not found")
				end
				if session.leaseId ~= nil and session.leaseId ~= currentRequestLeaseId() then
					error("Editor reconcile belongs to another request lease")
				end
				assertRequestLeaseActive(session.leaseId)
				if outerTransaction == nil then
					if session.rollbackSnapshot == nil then
						error("Editor reconcile session cannot be rolled back")
					end
					transactionSnapshot = session.rollbackSnapshot
				end
			end
		elseif #serviceNames > 0 and outerTransaction == nil then
			transactionSnapshot = TransactionState.captureSnapshot(serviceNames, params, ctx)
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
		local previousEditorTransaction = ctx.editorTransaction
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
		ctx.editorTransaction = outerTransaction
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
					stats.error = tostring(err)
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
					stats.error = tostring(err)
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
				stats.error = tostring(err)
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
						stats.error = tostring(err)
						aborted = true
						break
					end
					if os.clock() - sliceStarted >= 0.008 then
						task.wait()
						assertSessionOwnership(operationGeneration)
						assertReconcileActive()
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
				session.leaseId = currentRequestLeaseId()
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
		if aborted and outerTransaction == nil then
			finishHistoryRecording(historyRecording, Enum.FinishRecordingOperation.Cancel)
			historyRecording = nil
		end
		if aborted and transactionSnapshot ~= nil then
			if chunkSessionKey ~= nil then
				reconcileSessions[chunkSessionKey] = nil
			end
			local okRollback, replacements = pcall(function()
				if TransactionState.topologyMatchesSnapshot(transactionSnapshot) then
					TransactionState.restoreSnapshotState(transactionSnapshot, selectionReplacements, ctx)
					return selectionReplacements
				end
				return TransactionState.restoreSnapshot(transactionSnapshot, ctx, selectionReplacements)
			end)
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
		if outerTransaction == nil and historyRecording ~= nil then
			finishHistoryRecording(
				historyRecording,
				Enum.FinishRecordingOperation.Commit
			)
		end
		restoreExplorerSelection(explorerSelection, restoredSelectionReplacements)
		stats.lastMs = (os.clock() - started) * 1000
		for serviceName in pairs(touchedServices) do
			invalidateEditorService(serviceName)
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
		ctx.editorTransaction = previousEditorTransaction
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
		if not aborted and outerTransaction ~= nil and outerTransaction.mutated then
			outerTransaction.state = "prepared"
		end
		ctx.updateStatus()
		return stats
	end

	function api.requestCancellation()
		cancellationGeneration += 1
	end

	function api.cleanup()
		local activeTransactions = {}
		for transactionId, session in pairs(editorTransactions) do
			if type(session) == "table" and session.snapshot ~= nil then
				activeTransactions[#activeTransactions + 1] = {
					transactionId = transactionId,
					session = session,
				}
			end
		end
		for _, entry in ipairs(activeTransactions) do
			local session = entry.session
			local okRollback, rollbackError = pcall(runWithStudioChangeSuppression, ctx, function()
				return TransactionState.rollbackSession(session, ctx)
			end)
			if not okRollback then
				session.rollbackFailed = tostring(rollbackError)
				session.updatedAt = os.clock()
				warn("[Renium] transaction cleanup failed: " .. tostring(rollbackError))
			else
				session.onExpire = nil
				editorTransactions[entry.transactionId] = nil
				recordTransactionOutcome(entry.transactionId, "rolledBack", {
					replacements = countEntries(rollbackError),
				})
			end
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
				while session.activeSerializations > 0 do
					session.payloadReadyEvent.Event:Wait()
				end
				expireSession(binaryExports, exportId, session)
			else
				binaryExports[exportId] = nil
			end
		end
		if type(ctx.matchedSettingsInstancesByService) == "table" then
			table.clear(ctx.matchedSettingsInstancesByService)
		end
		if type(ctx.matchedSettingsIdByInstance) == "table" then
			table.clear(ctx.matchedSettingsIdByInstance)
			ctx.matchedSettingsIdVersion = (tonumber(ctx.matchedSettingsIdVersion) or 0) + 1
		end
	end

	return api
end

return BridgeEditorSync
