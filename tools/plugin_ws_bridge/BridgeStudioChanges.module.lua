

local BridgeStudioChanges = {}
local BridgeValueCodec = require(script.Parent.BridgeValueCodec)
local CHANGE_TRACKER_VERSION = 4
local CollectionService = game:GetService("CollectionService")
local MAX_CHANGE_LOGS_PER_SERVICE = 1024
local MAX_DIRECT_PROPERTY_CHANGES = 2048
local MAX_DIRECT_PROPERTY_BYTES = 8 * 1024 * 1024

type AllowedServices = { [string]: boolean }
type DirtySeqMap = { [string]: number }
type PropertyNameSetByClass = { [string]: { [string]: string } }
type ConnectionMap = { [Instance]: { RBXScriptConnection } }
type PropertyFingerprintMap = { [Instance]: { [string]: string } }
type DirectPropertyChange = {
	service: string,
	className: string,
	pathSegments: { string },
	pathOrdinals: { number },
	scope: string,
	property: string,
	value: any,
	seq: number,
	estimatedBytes: number,
}
type StudioChangeDetails = {
	action: string?,
	reason: string?,
	className: string?,
	path: string?,
	pathSegments: { string }?,
	pathOrdinals: { number }?,
	property: string?,
	attribute: string?,
	direct: boolean?,
	fullSync: boolean?,
}
type StudioChangeLog = {
	service: string,
	action: string,
	reason: string?,
	className: string?,
	path: string?,
	pathSegments: { string }?,
	pathOrdinals: { number }?,
	property: string?,
	attribute: string?,
	direct: boolean?,
	fullSync: boolean?,
	seq: number,
}

local ROOT_PROPERTY_IGNORES: { [string]: { [string]: boolean } } = {
	Workspace = {
		currentcamera = true,
		distributedgametime = true,
	},
}

local ALWAYS_RELEVANT_PROPERTIES: { [string]: boolean } = {
	name = true,
	parent = true,
	source = true,
	attributes = true,
	attributereplicate = true,
	attributesreplicate = true,
	attributesserialize = true,
}

local ALWAYS_IGNORED_PROPERTIES: { [string]: boolean } = {
	absoluteposition = true,
	absoluterotation = true,
	absolutesize = true,
	absolutecanvassize = true,
	absolutewindowsize = true,
	contenttext = true,
	textbounds = true,
	textfits = true,
	assemblycenterofmass = true,
	assemblylinearvelocity = true,
	assemblyangularvelocity = true,
	assemblymass = true,
	assemblyrootpart = true,
	currentphysicalproperties = true,
	extentscframe = true,
	extentssize = true,
	receiveage = true,
	playbackloudness = true,
	timelength = true,
	isloaded = true,
	isplaying = true,
}

local FULL_SYNC_PROPERTIES: { [string]: boolean } = {
	name = true,
	parent = true,
	attributes = true,
	attributereplicate = true,
	attributesreplicate = true,
	attributesserialize = true,
}

type State = {
	started: boolean,
	seq: number,
	dirtySeqByService: DirtySeqMap,
	fullSyncSeqByService: DirtySeqMap,
	propertyChangesByKey: { [string]: DirectPropertyChange },
	changeLogByKey: { [string]: StudioChangeLog },
	propertyFingerprintByInstance: PropertyFingerprintMap,
	ordinalCacheByParent: { [Instance]: { [Instance]: number } },
	watchedServices: { [string]: boolean },
	serviceRoots: { [string]: Instance },
	rootConnections: { [string]: { RBXScriptConnection } },
	globalConnections: { RBXScriptConnection },
	instanceConnections: ConnectionMap,
	itemChangedAvailable: boolean,
	tagSignalsAvailable: boolean,
	tagConnections: { [string]: { RBXScriptConnection } },
	taggedInstancesByTag: { [string]: { [Instance]: boolean } },
	changeEvent: BindableEvent,
	suppressUntil: number,
	suppressDepth: number,
	propertyNamesByClass: PropertyNameSetByClass?,
	propertyFilterClassCount: number,
	propertyFilterPropertyCount: number,
	connectedInstanceCount: number,
	conflictResolution: string,
	syncbackProperties: boolean,
	onlyCodeMode: boolean,
	changeLogCountByService: { [string]: number },
	directPropertyBytes: number,
	directPropertyCount: number,
	tagPollToken: number,
}

local function trim(value: string): string
	return string.gsub(value, "^%s*(.-)%s*$", "%1")
end

local function structuredPathKey(pathSegments: { string }?, pathOrdinals: { number }?): string
	if pathSegments == nil then
		return ""
	end
	local parts = table.create(#pathSegments)
	for index, segment in ipairs(pathSegments) do
		local ordinal = if pathOrdinals ~= nil then pathOrdinals[index] or 1 else 1
		parts[index] = string.format("%d:%s:%d", #segment, segment, ordinal)
	end
	return table.concat(parts, "|")
end

local function normalizeServices(rawServices: any, allowedServices: AllowedServices): { string }
	local requested = {}
	local seen = {}

	if type(rawServices) == "table" then
		local itemCount = 0
		for key, value in pairs(rawServices) do
			if type(key) ~= "number" or key % 1 ~= 0 or key < 1 or type(value) ~= "string" then
				error("Studio change services must be an array of service names")
			end
			itemCount += 1
			if not allowedServices[value] then
				error("Unsupported Studio change service: " .. value)
			end
			if not seen[value] then
				seen[value] = true
				requested[#requested + 1] = value
			end
		end
		if itemCount ~= #rawServices then
			error("Studio change services must be a dense array")
		end
	elseif type(rawServices) == "string" then
		for token in string.gmatch(rawServices, "[^,]+") do
			local serviceName = trim(token)
			if not allowedServices[serviceName] then
				error("Unsupported Studio change service: " .. serviceName)
			end
			if not seen[serviceName] then
				seen[serviceName] = true
				requested[#requested + 1] = serviceName
			end
		end
	elseif rawServices ~= nil then
		error("Studio change services must be an array or comma-separated string")
	end

	if #requested == 0 then
		if rawServices ~= nil and not (type(rawServices) == "string" and trim(rawServices) == "") then
			error("Studio change services cannot be empty")
		end
		for serviceName in pairs(allowedServices) do
			requested[#requested + 1] = serviceName
		end
	end
	table.sort(requested)
	return requested
end

function BridgeStudioChanges.create(config: { [string]: any }, allowedServices: AllowedServices)
	local state: State = {
		started = false,
		seq = 0,
		dirtySeqByService = {},
		fullSyncSeqByService = {},
		propertyChangesByKey = {},
		changeLogByKey = {},
		propertyFingerprintByInstance = setmetatable({}, { __mode = "k" }) :: any,
		ordinalCacheByParent = setmetatable({}, { __mode = "k" }) :: any,
		watchedServices = {},
		serviceRoots = {},
		rootConnections = {},
		globalConnections = {},
		instanceConnections = {},
		itemChangedAvailable = false,
		tagSignalsAvailable = false,
		tagConnections = {},
		taggedInstancesByTag = {},
		changeEvent = Instance.new("BindableEvent"),
		suppressUntil = 0,
		suppressDepth = 0,
		propertyNamesByClass = nil,
		propertyFilterClassCount = 0,
		propertyFilterPropertyCount = 0,
		connectedInstanceCount = 0,
		conflictResolution = "",
		syncbackProperties = true,
		onlyCodeMode = false,
		changeLogCountByService = {},
		directPropertyBytes = 0,
		directPropertyCount = 0,
		tagPollToken = 0,
	}

	local api = {}

	local function isSuppressed(): boolean
		return state.suppressDepth > 0 or os.clock() < state.suppressUntil
	end

	local function clearPropertyChangesForService(serviceName: string)
		for key, change in pairs(state.propertyChangesByKey) do
			if change.service == serviceName then
				state.directPropertyBytes = math.max(0, state.directPropertyBytes - (change.estimatedBytes or 0))
				state.directPropertyCount = math.max(0, state.directPropertyCount - 1)
				state.propertyChangesByKey[key] = nil
			end
		end
	end

	local function clearChangeLogsForService(serviceName: string)
		for key, change in pairs(state.changeLogByKey) do
			if change.service == serviceName then
				state.changeLogByKey[key] = nil
			end
		end
		state.changeLogCountByService[serviceName] = 0
	end

	local function signalChange()
		state.changeEvent:Fire(state.seq)
	end

	local function hasDirtyServices(services: { string }): boolean
		for _, serviceName in ipairs(services) do
			if state.dirtySeqByService[serviceName] ~= nil or state.fullSyncSeqByService[serviceName] ~= nil then
				return true
			end
		end
		return false
	end

	local function waitForDirtyServices(services: { string }, waitSeconds: number?): boolean
		local duration = tonumber(waitSeconds) or 0
		if duration <= 0 or hasDirtyServices(services) then
			return hasDirtyServices(services)
		end
		duration = math.min(duration, 25)

		local wakeEvent = Instance.new("BindableEvent")
		local done = false
		local timedOut = false
		local deadline = os.clock() + duration
		local connection = state.changeEvent.Event:Connect(function()
			if not done then
				wakeEvent:Fire("change")
				task.defer(function()
					if not done then
						wakeEvent:Fire("change")
					end
				end)
			end
		end)
		task.delay(duration, function()
			if not done then
				timedOut = true
				wakeEvent:Fire("timeout")
			end
		end)

		while not timedOut and os.clock() < deadline and not hasDirtyServices(services) do
			wakeEvent.Event:Wait()
		end

		done = true
		connection:Disconnect()
		wakeEvent:Destroy()
		return hasDirtyServices(services)
	end

	local function pathToString(pathSegments: { string }?): string?
		if pathSegments == nil or #pathSegments == 0 then
			return nil
		end
		return table.concat(pathSegments, ".")
	end

	local function recordChange(serviceName: string, seq: number, requiresFullSync: boolean, details: StudioChangeDetails?)
		local entry: StudioChangeLog = {
			service = serviceName,
			action = if requiresFullSync then "fullSync" else "property",
			seq = seq,
			fullSync = requiresFullSync,
		}
		if details ~= nil then
			entry.action = details.action or entry.action
			entry.reason = details.reason
			entry.className = details.className
			entry.path = details.path
			entry.pathSegments = details.pathSegments
			entry.pathOrdinals = details.pathOrdinals
			entry.property = details.property
			entry.attribute = details.attribute
			entry.direct = details.direct
			entry.fullSync = if details.fullSync ~= nil then details.fullSync else entry.fullSync
		end
		if entry.path == nil then
			entry.path = pathToString(entry.pathSegments) or serviceName
		end
		local structuredKey = structuredPathKey(entry.pathSegments, entry.pathOrdinals)
		local pathKey = if structuredKey == "" then entry.path or serviceName else structuredKey
		local key = serviceName
			.. "\0"
			.. entry.action
			.. "\0"
			.. tostring(pathKey)
			.. "\0"
			.. tostring(entry.property or entry.attribute or "")
		if state.changeLogByKey[key] == nil then
			local retainedCount = state.changeLogCountByService[serviceName] or 0
			if retainedCount >= MAX_CHANGE_LOGS_PER_SERVICE then
				clearChangeLogsForService(serviceName)
				clearPropertyChangesForService(serviceName)
				state.fullSyncSeqByService[serviceName] = seq
				entry = {
					service = serviceName,
					action = "fullSync",
					reason = "change log retention limit reached",
					path = serviceName,
					fullSync = true,
					seq = seq,
				}
				key = serviceName .. "\0fullSync\0retention-limit\0"
			end
			state.changeLogCountByService[serviceName] = (state.changeLogCountByService[serviceName] or 0) + 1
		end
		state.changeLogByKey[key] = entry
	end

	local function markDirty(serviceName: string?, requiresFullSync: boolean?, details: StudioChangeDetails?)
		if serviceName == nil or not allowedServices[serviceName] then
			return
		end
		if isSuppressed() then
			return
		end
		state.seq += 1
		state.dirtySeqByService[serviceName] = state.seq
		local isFullSync = requiresFullSync ~= false
		if isFullSync then
			state.fullSyncSeqByService[serviceName] = state.seq
			clearPropertyChangesForService(serviceName)
		end
		recordChange(serviceName, state.seq, isFullSync, details)
		signalChange()
	end

	local function directPropertyKey(serviceName: string, pathSegments: { string }, pathOrdinals: { number }, propertyName: string): string
		return serviceName .. "\0" .. structuredPathKey(pathSegments, pathOrdinals) .. "\0" .. propertyName
	end

	local function canTrackDirectProperty(propertyName: string): boolean
		return not FULL_SYNC_PROPERTIES[string.lower(propertyName)]
	end

	local function encodeDirectValue(value: any): (boolean, any)
		local valueType = type(value)
		if valueType == "boolean" or valueType == "string" then
			return true, value
		elseif valueType == "number" then
			return true, BridgeValueCodec.encodeNumber(value)
		end
		local robloxType = typeof(value)
		if robloxType == "Vector2" then
			local components = BridgeValueCodec.encodeComponents(value.X, value.Y)
			return true, { _type = "Vector2", x = components[1], y = components[2] }
		elseif robloxType == "Vector3" then
			local components = BridgeValueCodec.encodeComponents(value.X, value.Y, value.Z)
			return true, { _type = "Vector3", x = components[1], y = components[2], z = components[3] }
		elseif robloxType == "UDim" then
			local components = BridgeValueCodec.encodeComponents(value.Scale, value.Offset)
			return true, { _type = "UDim", scale = components[1], offset = components[2] }
		elseif robloxType == "UDim2" then
			local components =
				BridgeValueCodec.encodeComponents(value.X.Scale, value.X.Offset, value.Y.Scale, value.Y.Offset)
			return true, {
				_type = "UDim2",
				xScale = components[1],
				xOffset = components[2],
				yScale = components[3],
				yOffset = components[4],
			}
		elseif robloxType == "Color3" then
			local components = BridgeValueCodec.encodeComponents(value.R, value.G, value.B)
			return true, { _type = "Color3", r = components[1], g = components[2], b = components[3] }
		elseif robloxType == "BrickColor" then
			return true, { _type = "BrickColor", number = value.Number }
		elseif robloxType == "CFrame" then
			return true, { _type = "CFrame", components = BridgeValueCodec.encodeComponents(value:GetComponents()) }
		elseif robloxType == "EnumItem" then
			return true, { _type = "EnumItem", enumType = tostring(value.EnumType), name = value.Name }
		end
		return false, nil
	end

	local function encodeDirectPropertyValue(instance: Instance, propertyName: string): (boolean, any)
		local ok, value = pcall(function()
			return (instance :: any)[propertyName]
		end)
		if not ok then
			return false, nil
		end
		return encodeDirectValue(value)
	end

	local function estimatedValueBytes(value: any, depth: number?): number
		local currentDepth = depth or 0
		if currentDepth > 8 then
			return 16
		end
		local valueType = type(value)
		if valueType == "string" then
			return #value
		end
		if valueType == "number" or valueType == "boolean" or value == nil then
			return 16
		end
		if valueType == "table" then
			local total = 2
			for key, child in pairs(value) do
				total += estimatedValueBytes(key, currentDepth + 1) + estimatedValueBytes(child, currentDepth + 1) + 2
			end
			return total
		end
		return 64
	end

	local function invalidateSiblingOrdinals(parent: Instance?)
		if parent ~= nil then
			state.ordinalCacheByParent[parent] = nil
		end
	end

	local function siblingOrdinal(instance: Instance, parent: Instance): number
		local ordinals = state.ordinalCacheByParent[parent]
		if ordinals == nil then
			ordinals = setmetatable({}, { __mode = "k" }) :: any
			local counts = {}
			for _, child in ipairs(parent:GetChildren()) do
				local ordinal = (counts[child.Name] or 0) + 1
				counts[child.Name] = ordinal
				ordinals[child] = ordinal
			end
			state.ordinalCacheByParent[parent] = ordinals
		end
		return ordinals[instance] or 1
	end

	local function pathSegmentsAndOrdinalsForInstance(instance: Instance): ({ string }?, { number }?)
		if not instance:IsDescendantOf(game) then
			return nil, nil
		end
		local segments = {}
		local ordinals = {}
		local current: Instance? = instance
		while current ~= nil and current ~= game do
			local ordinal = 1
			local parent = current.Parent
			if parent ~= nil then
				ordinal = siblingOrdinal(current, parent)
			end
			table.insert(segments, 1, current.Name)
			table.insert(ordinals, 1, ordinal)
			current = parent
		end
		return segments, ordinals
	end

	local function changeDetailsForInstance(
		instance: Instance,
		action: string,
		propertyName: string?,
		attributeName: string?,
		reason: string?
	): StudioChangeDetails
		local pathSegments, pathOrdinals = pathSegmentsAndOrdinalsForInstance(instance)
		return {
			action = action,
			reason = reason,
			className = instance.ClassName,
			pathSegments = pathSegments,
			pathOrdinals = pathOrdinals,
			path = pathToString(pathSegments),
			property = propertyName,
			attribute = attributeName,
		}
	end

	local function markDirectProperty(
		instance: Instance,
		serviceName: string,
		propertyName: string,
		capturedOk: boolean?,
		capturedValue: any
	): boolean
		if isSuppressed() then
			return true
		end
		if not canTrackDirectProperty(propertyName) then
			return false
		end
		if state.fullSyncSeqByService[serviceName] ~= nil then
			markDirty(
				serviceName,
				true,
				changeDetailsForInstance(instance, "property", propertyName, nil, "property changed while full sync was pending")
			)
			return true
		end

		local pathSegments, pathOrdinals = pathSegmentsAndOrdinalsForInstance(instance)
		if pathSegments == nil or pathOrdinals == nil or #pathSegments == 0 or pathSegments[1] ~= serviceName then
			return false
		end
		local okValue, value
		if capturedOk ~= nil then
			okValue = capturedOk
			value = capturedValue
		else
			okValue, value = encodeDirectPropertyValue(instance, propertyName)
		end
		if not okValue then
			return false
		end
		local key = directPropertyKey(serviceName, pathSegments, pathOrdinals, propertyName)
		local previous = state.propertyChangesByKey[key]
		local previousBytes = if previous ~= nil then previous.estimatedBytes else 0
		local estimatedBytes = #key + estimatedValueBytes(value) + 128
		local nextCount = state.directPropertyCount + (if previous == nil then 1 else 0)
		local nextBytes = state.directPropertyBytes - previousBytes + estimatedBytes
		if nextCount > MAX_DIRECT_PROPERTY_CHANGES or nextBytes > MAX_DIRECT_PROPERTY_BYTES then
			markDirty(serviceName, true, {
				action = "fullSync",
				reason = "direct property retention limit reached",
				path = serviceName,
				fullSync = true,
			})
			return true
		end

		state.seq += 1
		state.dirtySeqByService[serviceName] = state.seq
		recordChange(serviceName, state.seq, false, {
			action = "property",
			reason = "property changed",
			className = instance.ClassName,
			pathSegments = pathSegments,
			pathOrdinals = pathOrdinals,
			path = pathToString(pathSegments),
			property = propertyName,
			direct = true,
			fullSync = false,
		})
		state.propertyChangesByKey[key] = {
			service = serviceName,
			className = instance.ClassName,
			pathSegments = pathSegments,
			pathOrdinals = pathOrdinals,
			scope = "property",
			property = propertyName,
			value = value,
			seq = state.seq,
			estimatedBytes = estimatedBytes,
		}
		state.directPropertyCount = nextCount
		state.directPropertyBytes = nextBytes
		signalChange()
		return true
	end

	local function shouldIgnoreInstance(instance: Instance): boolean
		local workspace = game:GetService("Workspace")
		local currentCamera = workspace.CurrentCamera
		if currentCamera == nil then
			return false
		end
		if instance == currentCamera then
			return true
		end
		return instance:IsDescendantOf(currentCamera)
	end

	local function isLuaSourceInstance(instance: Instance): boolean
		local luaSourceClasses = config.LUA_SOURCE_CLASS
		return type(luaSourceClasses) == "table" and luaSourceClasses[instance.ClassName] == true
	end

	local function hasLuaSourceDescendant(instance: Instance): boolean
		if isLuaSourceInstance(instance) then
			return true
		end
		return instance:FindFirstChildWhichIsA("LuaSourceContainer", true) ~= nil
	end

	local function exportPropertyNameForEvent(instance: Instance, loweredPropertyName: string): string
		if instance:IsA("BasePart") then
			if loweredPropertyName == "position" or loweredPropertyName == "orientation" or loweredPropertyName == "rotation" then
				return "cframe"
			end
		elseif instance:IsA("Model") or instance:IsA("WorldModel") then
			if loweredPropertyName == "worldpivotdata" then
				return "worldpivot"
			end
		end
		return loweredPropertyName
	end

	local function isRelevantInstanceProperty(instance: Instance, rawPropertyName: any): boolean
		if rawPropertyName == nil then
			return true
		end

		local propertyName = tostring(rawPropertyName)
		if propertyName == "" then
			return true
		end

		local lowered = string.lower(propertyName)
		if lowered == "source" then
			return isLuaSourceInstance(instance)
		end
		if not state.syncbackProperties then
			return false
		end
		if state.onlyCodeMode and not hasLuaSourceDescendant(instance) then
			return false
		end
		if ALWAYS_RELEVANT_PROPERTIES[lowered] then
			return true
		end
		if ALWAYS_IGNORED_PROPERTIES[lowered] then
			return false
		end

		local propertyNamesByClass = state.propertyNamesByClass
		if propertyNamesByClass == nil then
			return true
		end

		local classProperties = propertyNamesByClass[instance.ClassName]
		if classProperties == nil then
			return true
		end

		local exportPropertyName = exportPropertyNameForEvent(instance, lowered)
		return classProperties[exportPropertyName] ~= nil
	end

	local function serviceNameForTrackedInstance(instance: Instance): string?
		if shouldIgnoreInstance(instance) then
			return nil
		end
		for serviceName, service in pairs(state.serviceRoots) do
			if instance == service then
				return serviceName
			end
			if instance:IsDescendantOf(service) then
				return serviceName
			end
		end
		return nil
	end

	local function tagChangeRelevant(instance: Instance): boolean
		return not state.onlyCodeMode or hasLuaSourceDescendant(instance)
	end

	local function markTagChange(instance: Instance, tag: string, added: boolean)
		local serviceName = serviceNameForTrackedInstance(instance)
		if serviceName == nil or not tagChangeRelevant(instance) then
			return
		end
		markDirty(
			serviceName,
			true,
			changeDetailsForInstance(
				instance,
				"tag",
				"Tags",
				nil,
				if added then "tag added" else "tag removed"
			)
		)
	end

	local function connectTag(tag: string, markExisting: boolean)
		if state.tagConnections[tag] ~= nil then
			return
		end
		local tracked = setmetatable({}, { __mode = "k" }) :: any
		state.taggedInstancesByTag[tag] = tracked
		for _, instance in ipairs(CollectionService:GetTagged(tag)) do
			tracked[instance] = true
			if markExisting then
				markTagChange(instance, tag, true)
			end
		end
		local connections = {
			CollectionService:GetInstanceAddedSignal(tag):Connect(function(instance: Instance)
				if not tracked[instance] then
					tracked[instance] = true
					markTagChange(instance, tag, true)
				end
			end),
			CollectionService:GetInstanceRemovedSignal(tag):Connect(function(instance: Instance)
				if tracked[instance] then
					tracked[instance] = nil
					markTagChange(instance, tag, false)
				end
			end),
		}
		state.tagConnections[tag] = connections
		state.tagSignalsAvailable = true
	end

	local function discoverTags(markExisting: boolean)
		local tags = CollectionService:GetAllTags()
		local seen = {}
		for _, tag in ipairs(tags) do
			if type(tag) == "string" and tag ~= "" then
				seen[tag] = true
				connectTag(tag, markExisting)
			end
		end
		for tag, connections in pairs(state.tagConnections) do
			if not seen[tag] then
				for _, connection in ipairs(connections) do
					connection:Disconnect()
				end
				state.tagConnections[tag] = nil
				state.taggedInstancesByTag[tag] = nil
			end
		end
	end

	local function shouldIgnoreRootProperty(service: Instance, serviceName: string, propertyName: string): boolean
		local lowered = string.lower(propertyName)
		local ignoredProperties = ROOT_PROPERTY_IGNORES[serviceName]
		if ignoredProperties ~= nil and ignoredProperties[lowered] then
			return true
		end
		return not isRelevantInstanceProperty(service, propertyName)
	end

	local function stringFingerprint(value: string): string
		local first = 5381
		local second = 2166136261
		for index = 1, #value do
			local byte = string.byte(value, index)
			first = (first * 33 + byte) % 4294967296
			second = (second * 65599 + byte) % 4294967296
		end
		return string.format("%d:%08x%08x", #value, first, second)
	end

	local function stableValueString(value: any, depth: number?): string
		local currentDepth = depth or 0
		if currentDepth > 8 then
			return "<max-depth>"
		end

		local valueType = type(value)
		if value == nil then
			return "nil"
		elseif valueType == "string" then
			return "string:" .. stringFingerprint(value)
		elseif valueType == "boolean" or valueType == "number" then
			return valueType .. ":" .. tostring(value)
		elseif valueType == "table" then
			local parts = {}
			for key, child in pairs(value) do
				parts[#parts + 1] = stableValueString(key, currentDepth + 1) .. "=" .. stableValueString(child, currentDepth + 1)
			end
			table.sort(parts)
			return "table:{" .. table.concat(parts, ",") .. "}"
		end

		local robloxType = typeof(value)
		if robloxType == "Vector2" then
			return ("Vector2:%s,%s"):format(tostring(value.X), tostring(value.Y))
		elseif robloxType == "Vector3" then
			return ("Vector3:%s,%s,%s"):format(tostring(value.X), tostring(value.Y), tostring(value.Z))
		elseif robloxType == "UDim" then
			return ("UDim:%s,%s"):format(tostring(value.Scale), tostring(value.Offset))
		elseif robloxType == "UDim2" then
			return ("UDim2:%s,%s,%s,%s"):format(
				tostring(value.X.Scale),
				tostring(value.X.Offset),
				tostring(value.Y.Scale),
				tostring(value.Y.Offset)
			)
		elseif robloxType == "Color3" then
			return ("Color3:%s,%s,%s"):format(tostring(value.R), tostring(value.G), tostring(value.B))
		elseif robloxType == "BrickColor" then
			return "BrickColor:" .. tostring(value.Number)
		elseif robloxType == "CFrame" then
			local components = { value:GetComponents() }
			for index, component in ipairs(components) do
				components[index] = tostring(component)
			end
			return "CFrame:" .. table.concat(components, ",")
		elseif robloxType == "EnumItem" then
			return "EnumItem:" .. tostring(value.EnumType) .. "." .. value.Name
		elseif robloxType == "Instance" then
			local pathSegments, pathOrdinals = pathSegmentsAndOrdinalsForInstance(value)
			local pathKey = structuredPathKey(pathSegments, pathOrdinals)
			return "Instance:" .. (if pathKey ~= "" then pathKey else tostring(value))
		end

		return robloxType .. ":" .. tostring(value)
	end

	local function propertyCacheKey(instance: Instance, propertyName: string): string
		local lowered = string.lower(propertyName)
		if lowered == "attributes" or lowered == "attributereplicate" or lowered == "attributesreplicate" or lowered == "attributesserialize" then
			return "attributes"
		end
		return "property:" .. exportPropertyNameForEvent(instance, lowered)
	end

	local function propertyReadNameForEvent(instance: Instance, propertyName: string): string
		local lowered = string.lower(propertyName)
		if instance:IsA("BasePart") then
			if lowered == "position" or lowered == "orientation" or lowered == "rotation" then
				return "CFrame"
			end
		elseif instance:IsA("Model") or instance:IsA("WorldModel") then
			if lowered == "worldpivotdata" then
				return "WorldPivot"
			end
		end

		local propertyNamesByClass = state.propertyNamesByClass
		if propertyNamesByClass ~= nil then
			local classProperties = propertyNamesByClass[instance.ClassName]
			if classProperties ~= nil then
				local configuredName = classProperties[exportPropertyNameForEvent(instance, lowered)]
				if type(configuredName) == "string" and configuredName ~= "" then
					return configuredName
				end
			end
		end

		return propertyName
	end

	local function readPropertyFingerprint(instance: Instance, propertyName: string): (string?, boolean, any)
		local lowered = string.lower(propertyName)
		if lowered == "attributes" or lowered == "attributereplicate" or lowered == "attributesreplicate" or lowered == "attributesserialize" then
			local attributes = instance:GetAttributes()
			return stableValueString(attributes), false, nil
		end
		if lowered == "parent" then
			local pathSegments, pathOrdinals = pathSegmentsAndOrdinalsForInstance(instance)
			return stableValueString({
				pathSegments = pathSegments or {},
				pathOrdinals = pathOrdinals or {},
			}), false, nil
		end

		local readName = propertyReadNameForEvent(instance, propertyName)
		local okValue, value = pcall(function()
			return (instance :: any)[readName]
		end)
		if not okValue and readName ~= propertyName then
			okValue, value = pcall(function()
				return (instance :: any)[propertyName]
			end)
		end
		if not okValue then
			return nil, false, nil
		end
		local directOk, directValue = encodeDirectValue(value)
		return stableValueString(value), directOk, directValue
	end

	local function shouldRecordPropertyDirty(instance: Instance, propertyName: string): (boolean, boolean, any)
		local fingerprint, directOk, directValue = readPropertyFingerprint(instance, propertyName)
		if fingerprint == nil then
			return true, false, nil
		end

		local cache = state.propertyFingerprintByInstance[instance]
		if cache == nil then
			cache = {}
			state.propertyFingerprintByInstance[instance] = cache
		end
		local key = propertyCacheKey(instance, propertyName)
		local previous = cache[key]
		cache[key] = fingerprint
		return previous == nil or previous ~= fingerprint, directOk, directValue
	end

	local function shouldRecordAttributeDirty(instance: Instance, attributeName: string): boolean
		local value = instance:GetAttribute(attributeName)
		local cache = state.propertyFingerprintByInstance[instance]
		if cache == nil then
			cache = {}
			state.propertyFingerprintByInstance[instance] = cache
		end
		local key = "attribute:" .. attributeName
		local fingerprint = stableValueString(value)
		local previous = cache[key]
		cache[key] = fingerprint
		return previous == nil or previous ~= fingerprint
	end

	local function connectAttributeChanged(instance: Instance, serviceName: string): RBXScriptConnection?
		return instance.AttributeChanged:Connect(function(attributeName: string)
				if not state.syncbackProperties then
					return
				end
				if state.onlyCodeMode and not hasLuaSourceDescendant(instance) then
					return
				end
				local attribute = tostring(attributeName)
				if shouldRecordAttributeDirty(instance, attribute) then
					markDirty(
						serviceName,
						true,
						changeDetailsForInstance(instance, "attribute", nil, attribute, "attribute changed")
					)
				end
			end)
	end

	local function disconnectInstance(instance: Instance)
		local connections = state.instanceConnections[instance]
		if connections == nil then
			return
		end
		state.instanceConnections[instance] = nil
		state.propertyFingerprintByInstance[instance] = nil
		state.connectedInstanceCount = math.max(0, state.connectedInstanceCount - 1)
		for _, connection in ipairs(connections) do
			connection:Disconnect()
		end
	end

	local function disconnectInstanceTree(instance: Instance)
		for _, descendant in ipairs(instance:GetDescendants()) do
			disconnectInstance(descendant)
		end
		disconnectInstance(instance)
	end

	local function connectInstance(instance: Instance, serviceName: string)
		if state.instanceConnections[instance] ~= nil or shouldIgnoreInstance(instance) then
			return
		end

		local connections: { RBXScriptConnection } = {}
		local changedConnection = instance.Changed:Connect(function(propertyName: any)
				local dirtyPropertyName = if instance:IsA("ValueBase") then "Value" else propertyName
				if isRelevantInstanceProperty(instance, dirtyPropertyName) then
					local property = tostring(dirtyPropertyName)
					local lowered = string.lower(property)
					if lowered == "name" or lowered == "parent" then
						invalidateSiblingOrdinals(instance.Parent)
					end
					local shouldRecord, directOk, directValue = shouldRecordPropertyDirty(instance, property)
					if not shouldRecord then
						return
					end
					if not markDirectProperty(instance, serviceName, property, directOk, directValue) then
						markDirty(
							serviceName,
							true,
							changeDetailsForInstance(instance, "property", property, nil, "property changed")
						)
					end
				end
			end)
		table.insert(connections, changedConnection)

		local attributeConnection = connectAttributeChanged(instance, serviceName)
		if attributeConnection ~= nil then
			table.insert(connections, attributeConnection)
		end

		if #connections > 0 then
			state.instanceConnections[instance] = connections
			state.connectedInstanceCount += 1
		end
	end

	local function connectExistingDescendants(service: Instance, serviceName: string)
		for _, descendant in ipairs(service:GetDescendants()) do
			if not state.onlyCodeMode or hasLuaSourceDescendant(descendant) then
				connectInstance(descendant, serviceName)
			end
		end
	end

	local function reconcileServiceConnections(service: Instance, serviceName: string)
		local descendants = service:GetDescendants()
		local desired = {}
		for _, descendant in ipairs(descendants) do
			if not shouldIgnoreInstance(descendant) and (not state.onlyCodeMode or hasLuaSourceDescendant(descendant)) then
				desired[descendant] = true
				connectInstance(descendant, serviceName)
			end
		end
		local disconnect = {}
		for instance in pairs(state.instanceConnections) do
			if instance:IsDescendantOf(service) and not desired[instance] then
				table.insert(disconnect, instance)
			end
		end
		for _, instance in ipairs(disconnect) do
			disconnectInstance(instance)
		end
	end

	local function reconcileAncestorConnections(instance: Instance, service: Instance, serviceName: string)
		local current = instance.Parent
		while current ~= nil and current ~= service do
			if not state.onlyCodeMode or hasLuaSourceDescendant(current) then
				connectInstance(current, serviceName)
			else
				disconnectInstance(current)
			end
			current = current.Parent
		end
	end

	local function ensureService(serviceName: string)
		if state.watchedServices[serviceName] then
			return
		end
		local service = game:GetService(serviceName)
		state.watchedServices[serviceName] = true
		state.serviceRoots[serviceName] = service

		local connections: { RBXScriptConnection } = {
			service.Changed:Connect(function(propertyName: string)
				local property = tostring(propertyName)
				if not shouldIgnoreRootProperty(service, serviceName, property) then
					local shouldRecord, directOk, directValue = shouldRecordPropertyDirty(service, property)
					if not shouldRecord then
						return
					end
					if not markDirectProperty(service, serviceName, property, directOk, directValue) then
						markDirty(
							serviceName,
							true,
							changeDetailsForInstance(service, "property", property, nil, "service property changed")
						)
					end
				end
			end),
			service.DescendantAdded:Connect(function(instance: Instance)
				invalidateSiblingOrdinals(instance.Parent)
				if not shouldIgnoreInstance(instance) and (not state.onlyCodeMode or hasLuaSourceDescendant(instance)) then
					connectInstance(instance, serviceName)
					reconcileAncestorConnections(instance, service, serviceName)
					markDirty(
						serviceName,
						true,
						changeDetailsForInstance(instance, "added", nil, nil, "descendant added")
					)
				end
			end),
			service.DescendantRemoving:Connect(function(instance: Instance)
				if state.instanceConnections[instance] == nil then
					return
				end
				local ancestors = {}
				local current = instance.Parent
				while current ~= nil and current ~= service do
					table.insert(ancestors, current)
					current = current.Parent
				end
				invalidateSiblingOrdinals(instance.Parent)
				if not shouldIgnoreInstance(instance) and (not state.onlyCodeMode or hasLuaSourceDescendant(instance)) then
					markDirty(
						serviceName,
						true,
						changeDetailsForInstance(instance, "removed", nil, nil, "descendant removing")
					)
				end
				disconnectInstanceTree(instance)
				if state.onlyCodeMode and #ancestors > 0 then
					task.defer(function()
						for _, ancestor in ipairs(ancestors) do
							if ancestor:IsDescendantOf(service) and not hasLuaSourceDescendant(ancestor) then
								disconnectInstance(ancestor)
							end
						end
					end)
				end
			end),
		}

		local rootAttributeConnection = connectAttributeChanged(service, serviceName)
		if rootAttributeConnection ~= nil then
			table.insert(connections, rootAttributeConnection)
		end
		state.rootConnections[serviceName] = connections
		connectExistingDescendants(service, serviceName)
	end

	local function unwatchService(serviceName: string)
		local service = state.serviceRoots[serviceName]
		if service == nil then
			return
		end
		for _, connection in ipairs(state.rootConnections[serviceName] or {}) do
			connection:Disconnect()
		end
		state.rootConnections[serviceName] = nil
		local disconnect = {}
		for instance in pairs(state.instanceConnections) do
			if instance:IsDescendantOf(service) then
				table.insert(disconnect, instance)
			end
		end
		for _, instance in ipairs(disconnect) do
			disconnectInstance(instance)
		end
		state.watchedServices[serviceName] = nil
		state.serviceRoots[serviceName] = nil
		state.dirtySeqByService[serviceName] = nil
		state.fullSyncSeqByService[serviceName] = nil
		clearPropertyChangesForService(serviceName)
		clearChangeLogsForService(serviceName)
	end

	local function stopTracking()
		state.started = false
		state.tagPollToken += 1
		local watched = {}
		for serviceName in pairs(state.watchedServices) do
			table.insert(watched, serviceName)
		end
		for _, serviceName in ipairs(watched) do
			unwatchService(serviceName)
		end
		for _, connection in ipairs(state.globalConnections) do
			connection:Disconnect()
		end
		table.clear(state.globalConnections)
		for _, connections in pairs(state.tagConnections) do
			for _, connection in ipairs(connections) do
				connection:Disconnect()
			end
		end
		table.clear(state.tagConnections)
		table.clear(state.taggedInstancesByTag)
		state.itemChangedAvailable = false
		state.tagSignalsAvailable = false
	end

	local function ensureTracking(services: { string })
		if config.bridgeRole ~= "edit" then
			return
		end
		for _, serviceName in ipairs(services) do
			ensureService(serviceName)
		end
		if not state.started then
			local itemChanged = (game :: any).ItemChanged
			if itemChanged then
				state.globalConnections[#state.globalConnections + 1] = itemChanged:Connect(function(instance: Instance, propertyName: any)
					if typeof(instance) == "Instance" and string.lower(tostring(propertyName or "")) == "tags" then
						markTagChange(instance, "Tags", true)
					end
				end)
				state.itemChangedAvailable = true
			end
			discoverTags(false)
			state.started = true
			state.tagPollToken += 1
			local pollToken = state.tagPollToken
			task.spawn(function()
				while state.started and state.tagPollToken == pollToken do
					task.wait(2)
					if state.started and state.tagPollToken == pollToken then
						discoverTags(true)
					end
				end
			end)
		end
	end

	function api.configurePropertyCandidates(rawCandidatesByClass: any): { [string]: any }
		if type(rawCandidatesByClass) ~= "table" then
			state.propertyNamesByClass = nil
			state.propertyFilterClassCount = 0
			state.propertyFilterPropertyCount = 0
			return { ok = true, classes = 0, properties = 0 }
		end

		local normalized: PropertyNameSetByClass = {}
		local classCount = 0
		local propertyCount = 0
		for className, propertyNames in pairs(rawCandidatesByClass) do
			if type(className) == "string" and type(propertyNames) == "table" then
				local set: { [string]: string } = {}
				local countForClass = 0
				for _, propertyName in ipairs(propertyNames) do
					if type(propertyName) == "string" and propertyName ~= "" then
						local lowered = string.lower(propertyName)
						if set[lowered] == nil then
							set[lowered] = propertyName
							countForClass += 1
						end
					end
				end
				if countForClass > 0 then
					normalized[className] = set
					classCount += 1
					propertyCount += countForClass
				end
			end
		end

		state.propertyNamesByClass = normalized
		state.propertyFilterClassCount = classCount
		state.propertyFilterPropertyCount = propertyCount
		return { ok = true, classes = classCount, properties = propertyCount }
	end

	function api.setConflictResolution(value: string): string
		if value ~= "prompt" and value ~= "filesystem" and value ~= "studio" then
			error("Conflict resolution must be prompt, filesystem, or studio")
		end
		state.conflictResolution = value
		return value
	end

	function api.setOptions(rawOptions: any)
		if type(rawOptions) ~= "table" then
			return
		end
		if type(rawOptions.syncbackProperties) == "boolean" then
			state.syncbackProperties = rawOptions.syncbackProperties
		end
		if type(rawOptions.onlyCodeMode) == "boolean" and state.onlyCodeMode ~= rawOptions.onlyCodeMode then
			state.onlyCodeMode = rawOptions.onlyCodeMode
			for serviceName, service in pairs(state.serviceRoots) do
				reconcileServiceConnections(service, serviceName)
			end
		end
	end

	function api.getConflictResolution(): string
		return state.conflictResolution
	end

	function api.suppress(seconds: number?)
		local duration = tonumber(seconds) or 0.2
		if duration <= 0 then
			return
		end
		state.suppressUntil = math.max(state.suppressUntil, os.clock() + duration)
	end

	function api.beginSuppress(seconds: number?)
		state.suppressDepth += 1
		local duration = tonumber(seconds)
		if duration ~= nil and duration > 0 then
			api.suppress(duration)
		end
	end

	function api.endSuppress(settleSeconds: number?)
		if state.suppressDepth > 0 then
			state.suppressDepth -= 1
		else
			state.suppressDepth = 0
		end
		local duration = tonumber(settleSeconds)
		if duration ~= nil and duration > 0 then
			api.suppress(duration)
		end
	end

	local function applyStateParams(params: { [string]: any }, services: { string })
		local suppressSeconds = tonumber(params.suppressSeconds)
		if suppressSeconds ~= nil and suppressSeconds > 0 then
			api.suppress(suppressSeconds)
		end

		local ackSeq = tonumber(params.ackSeq)
		if ackSeq ~= nil then
			if type(params.runtimeId) ~= "string" or params.runtimeId ~= config.bridgeRuntimeId then
				error("Studio change acknowledgment runtime does not match the active plugin runtime")
			end
			for _, serviceName in ipairs(services) do
				local dirtySeq = state.dirtySeqByService[serviceName]
				if dirtySeq ~= nil and dirtySeq <= ackSeq then
					state.dirtySeqByService[serviceName] = nil
				end
				local fullSyncSeq = state.fullSyncSeqByService[serviceName]
				if fullSyncSeq ~= nil and fullSyncSeq <= ackSeq then
					state.fullSyncSeqByService[serviceName] = nil
				end
				for key, change in pairs(state.propertyChangesByKey) do
					if change.service == serviceName and change.seq <= ackSeq then
						state.directPropertyBytes =
							math.max(0, state.directPropertyBytes - (change.estimatedBytes or 0))
						state.directPropertyCount = math.max(0, state.directPropertyCount - 1)
						state.propertyChangesByKey[key] = nil
					end
				end
				for key, change in pairs(state.changeLogByKey) do
					if change.service == serviceName and change.seq <= ackSeq then
						state.changeLogByKey[key] = nil
						state.changeLogCountByService[serviceName] =
							math.max(0, (state.changeLogCountByService[serviceName] or 0) - 1)
					end
				end
			end
		end
		if params.reset == true then
			for _, serviceName in ipairs(services) do
				state.dirtySeqByService[serviceName] = nil
				state.fullSyncSeqByService[serviceName] = nil
				clearPropertyChangesForService(serviceName)
				clearChangeLogsForService(serviceName)
			end
		end
	end

	local function buildStateResponse(services: { string }): { [string]: any }
		local requested = {}
		for _, serviceName in ipairs(services) do
			requested[serviceName] = true
		end
		local dirtyServices = {}
		local fullSyncServices = {}
		for _, serviceName in ipairs(services) do
			if state.dirtySeqByService[serviceName] ~= nil then
				dirtyServices[#dirtyServices + 1] = serviceName
			end
			if state.fullSyncSeqByService[serviceName] ~= nil then
				fullSyncServices[#fullSyncServices + 1] = serviceName
			end
		end
		local propertyChanges = {}
		for _, change in pairs(state.propertyChangesByKey) do
			if
				requested[change.service]
				and state.dirtySeqByService[change.service] ~= nil
				and state.fullSyncSeqByService[change.service] == nil
			then
				propertyChanges[#propertyChanges + 1] = change
			end
		end
		table.sort(propertyChanges, function(a, b)
			return a.seq < b.seq
		end)
		local changes = {}
		for _, change in pairs(state.changeLogByKey) do
			if requested[change.service] and state.dirtySeqByService[change.service] ~= nil then
				changes[#changes + 1] = change
			end
		end
		table.sort(changes, function(a, b)
			return a.seq < b.seq
		end)
		if #changes == 0 and #dirtyServices > 0 then
			for _, serviceName in ipairs(dirtyServices) do
				changes[#changes + 1] = {
					service = serviceName,
					action = "fullSync",
					reason = "dirty service had no retained change log",
					path = serviceName,
					fullSync = true,
					seq = state.dirtySeqByService[serviceName] or state.seq,
				}
			end
		end
		local trackedServiceCount = 0
		for _ in pairs(state.watchedServices) do
			trackedServiceCount += 1
		end
		return {
			ok = true,
			tracking = state.started,
			role = config.bridgeRole,
			changeTrackerVersion = CHANGE_TRACKER_VERSION,
			runtimeId = config.bridgeRuntimeId,
			seq = state.seq,
			dirtyServices = dirtyServices,
			fullSyncServices = fullSyncServices,
			propertyChanges = propertyChanges,
			changes = changes,
			itemChangedAvailable = state.itemChangedAvailable,
			tagSignalsAvailable = state.tagSignalsAvailable,
			propertyFilterClasses = state.propertyFilterClassCount,
			propertyFilterProperties = state.propertyFilterPropertyCount,
			connectedInstances = state.connectedInstanceCount,
			trackedServices = trackedServiceCount,
			conflictResolution = state.conflictResolution,
			syncbackProperties = state.syncbackProperties,
			onlyCodeMode = state.onlyCodeMode,
		}
	end

	function api.getState(params: { [string]: any }): { [string]: any }
		local services = normalizeServices(params.services, allowedServices)
		if params.stop == true then
			stopTracking()
		elseif params.start ~= false then
			if params.reset == true then
				local requested = {}
				for _, serviceName in ipairs(services) do
					requested[serviceName] = true
				end
				local removed = {}
				for serviceName in pairs(state.watchedServices) do
					if not requested[serviceName] then
						table.insert(removed, serviceName)
					end
				end
				for _, serviceName in ipairs(removed) do
					unwatchService(serviceName)
				end
			end
			ensureTracking(services)
		end
		applyStateParams(params, services)

		local waitSeconds = tonumber(params.waitSeconds)
		local waitedForChange = false
		local waitTimedOut = false
		if waitSeconds ~= nil and waitSeconds > 0 and params.reset ~= true and params.ackSeq == nil then
			waitedForChange = true
			waitTimedOut = not waitForDirtyServices(services, waitSeconds)
		end

		local response = buildStateResponse(services)
		if waitedForChange then
			response.eventDriven = true
			response.waitSeconds = math.min(waitSeconds or 0, 25)
			response.waitTimedOut = waitTimedOut
		end
		return response
	end

	function api.stop()
		stopTracking()
		state.changeEvent:Destroy()
	end

	return api
end

return BridgeStudioChanges
