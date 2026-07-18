

local BridgeStudioChanges = {}
local CHANGE_TRACKER_VERSION = 4
local CollectionService = game:GetService("CollectionService")

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
}

local function trim(value: string): string
	return string.gsub(value, "^%s*(.-)%s*$", "%1")
end

local function normalizeServices(rawServices: any, allowedServices: AllowedServices): { string }
	local requested = {}
	local seen = {}

	if type(rawServices) == "table" then
		for _, value in pairs(rawServices) do
			local serviceName = tostring(value)
			if allowedServices[serviceName] and not seen[serviceName] then
				seen[serviceName] = true
				requested[#requested + 1] = serviceName
			end
		end
	elseif type(rawServices) == "string" then
		for token in string.gmatch(rawServices, "[^,]+") do
			local serviceName = trim(token)
			if allowedServices[serviceName] and not seen[serviceName] then
				seen[serviceName] = true
				requested[#requested + 1] = serviceName
			end
		end
	end

	if #requested == 0 then
		for serviceName in pairs(allowedServices) do
			requested[#requested + 1] = serviceName
		end
	end
	table.sort(requested)
	return requested
end

local function safeIsA(instance: Instance, className: string): boolean
	local ok, result = pcall(function()
		return instance:IsA(className)
	end)
	return ok and result == true
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
	}

	local api = {}

	local function isSuppressed(): boolean
		return state.suppressDepth > 0 or os.clock() < state.suppressUntil
	end

	local function clearPropertyChangesForService(serviceName: string)
		for key, change in pairs(state.propertyChangesByKey) do
			if change.service == serviceName then
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
		pcall(function()
			connection:Disconnect()
		end)
		pcall(function()
			wakeEvent:Destroy()
		end)
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
		local pathKey = entry.path or serviceName
		if entry.pathOrdinals ~= nil and #entry.pathOrdinals > 0 then
			pathKey = pathKey .. "\0" .. table.concat(entry.pathOrdinals, ",")
		end
		local key = serviceName
			.. "\0"
			.. entry.action
			.. "\0"
			.. tostring(pathKey)
			.. "\0"
			.. tostring(entry.property or entry.attribute or "")
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
		return serviceName .. "\0" .. (pathToString(pathSegments) or "") .. "\0" .. table.concat(pathOrdinals, ",") .. "\0" .. propertyName
	end

	local function canTrackDirectProperty(propertyName: string): boolean
		return FULL_SYNC_PROPERTIES[string.lower(propertyName)] ~= true
	end

	local function encodeDirectPropertyValue(instance: Instance, propertyName: string): (boolean, any)
		local ok, value = pcall(function()
			return (instance :: any)[propertyName]
		end)
		if not ok then
			return false, nil
		end
		local valueType = type(value)
		if valueType == "boolean" or valueType == "number" or valueType == "string" then
			return true, value
		end
		local robloxType = typeof(value)
		if robloxType == "Vector2" then
			return true, { _type = "Vector2", x = value.X, y = value.Y }
		elseif robloxType == "Vector3" then
			return true, { _type = "Vector3", x = value.X, y = value.Y, z = value.Z }
		elseif robloxType == "UDim" then
			return true, { _type = "UDim", scale = value.Scale, offset = value.Offset }
		elseif robloxType == "UDim2" then
			return true, {
				_type = "UDim2",
				xScale = value.X.Scale,
				xOffset = value.X.Offset,
				yScale = value.Y.Scale,
				yOffset = value.Y.Offset,
			}
		elseif robloxType == "Color3" then
			return true, { _type = "Color3", r = value.R, g = value.G, b = value.B }
		elseif robloxType == "BrickColor" then
			return true, { _type = "BrickColor", number = value.Number }
		elseif robloxType == "CFrame" then
			return true, { _type = "CFrame", components = { value:GetComponents() } }
		elseif robloxType == "EnumItem" then
			return true, { _type = "EnumItem", enumType = tostring(value.EnumType), name = value.Name }
		end
		return false, nil
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
				ordinal = 0
				for _, child in ipairs(parent:GetChildren()) do
					if child.Name == current.Name then
						ordinal += 1
						if child == current then
							break
						end
					end
				end
				if ordinal < 1 then
					ordinal = 1
				end
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

	local function markDirectProperty(instance: Instance, serviceName: string, propertyName: string): boolean
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
		local okValue, value = encodeDirectPropertyValue(instance, propertyName)
		if not okValue then
			return false
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
		state.propertyChangesByKey[directPropertyKey(serviceName, pathSegments, pathOrdinals, propertyName)] = {
			service = serviceName,
			className = instance.ClassName,
			pathSegments = pathSegments,
			pathOrdinals = pathOrdinals,
			scope = "property",
			property = propertyName,
			value = value,
			seq = state.seq,
		}
		signalChange()
		return true
	end

	local function shouldIgnoreInstance(instance: Instance): boolean
		if safeIsA(instance, "Camera") then
			return true
		end

		local workspace = game:GetService("Workspace")
		local currentCamera = workspace.CurrentCamera
		if currentCamera == nil then
			return false
		end
		if instance == currentCamera then
			return true
		end
		local ok, isDescendant = pcall(function()
			return instance:IsDescendantOf(currentCamera)
		end)
		return ok and isDescendant
	end

	local function isLuaSourceInstance(instance: Instance): boolean
		local luaSourceClasses = config.LUA_SOURCE_CLASS
		return type(luaSourceClasses) == "table" and luaSourceClasses[instance.ClassName] == true
	end

	local function hasLuaSourceDescendant(instance: Instance): boolean
		if isLuaSourceInstance(instance) then
			return true
		end
		local ok, descendant = pcall(function()
			return instance:FindFirstChildWhichIsA("LuaSourceContainer", true)
		end)
		return ok and descendant ~= nil
	end

	local function exportPropertyNameForEvent(instance: Instance, loweredPropertyName: string): string
		if safeIsA(instance, "BasePart") then
			if loweredPropertyName == "position" or loweredPropertyName == "orientation" or loweredPropertyName == "rotation" then
				return "cframe"
			end
		elseif safeIsA(instance, "Model") or safeIsA(instance, "WorldModel") then
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
		if ALWAYS_RELEVANT_PROPERTIES[lowered] == true then
			return true
		end
		if ALWAYS_IGNORED_PROPERTIES[lowered] == true then
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

	local function serviceNameForDescendant(instance: Instance): string?
		if shouldIgnoreInstance(instance) then
			return nil
		end
		for serviceName, service in pairs(state.serviceRoots) do
			if instance ~= service then
				local ok, isDescendant = pcall(function()
					return instance:IsDescendantOf(service)
				end)
				if ok and isDescendant then
					return serviceName
				end
			end
		end
		return nil
	end

	local function serviceNameForTrackedInstance(instance: Instance): string?
		if shouldIgnoreInstance(instance) then
			return nil
		end
		for serviceName, service in pairs(state.serviceRoots) do
			if instance == service then
				return serviceName
			end
			local ok, isDescendant = pcall(function()
				return instance:IsDescendantOf(service)
			end)
			if ok and isDescendant then
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
		local okTagged, tagged = pcall(CollectionService.GetTagged, CollectionService, tag)
		if okTagged and type(tagged) == "table" then
			for _, instance in ipairs(tagged) do
				if typeof(instance) == "Instance" then
					tracked[instance] = true
					if markExisting then
						markTagChange(instance, tag, true)
					end
				end
			end
		end
		local connections = {}
		local okAdded, addedSignal = pcall(CollectionService.GetInstanceAddedSignal, CollectionService, tag)
		if okAdded and addedSignal ~= nil then
			connections[#connections + 1] = addedSignal:Connect(function(instance: Instance)
				if tracked[instance] ~= true then
					tracked[instance] = true
					markTagChange(instance, tag, true)
				end
			end)
		end
		local okRemoved, removedSignal = pcall(CollectionService.GetInstanceRemovedSignal, CollectionService, tag)
		if okRemoved and removedSignal ~= nil then
			connections[#connections + 1] = removedSignal:Connect(function(instance: Instance)
				if tracked[instance] == true then
					tracked[instance] = nil
					markTagChange(instance, tag, false)
				end
			end)
		end
		if #connections > 0 then
			state.tagConnections[tag] = connections
			state.tagSignalsAvailable = true
		else
			state.taggedInstancesByTag[tag] = nil
		end
	end

	local function discoverTags(markExisting: boolean)
		local okTags, tags = pcall(CollectionService.GetAllTags, CollectionService)
		if not okTags or type(tags) ~= "table" then
			return
		end
		for _, tag in ipairs(tags) do
			if type(tag) == "string" and tag ~= "" then
				connectTag(tag, markExisting)
			end
		end
	end

	local function shouldIgnoreRootProperty(service: Instance, serviceName: string, propertyName: string): boolean
		local lowered = string.lower(propertyName)
		local ignoredProperties = ROOT_PROPERTY_IGNORES[serviceName]
		if ignoredProperties ~= nil and ignoredProperties[lowered] == true then
			return true
		end
		return not isRelevantInstanceProperty(service, propertyName)
	end

	local function stableValueString(value: any, depth: number?): string
		local currentDepth = depth or 0
		if currentDepth > 8 then
			return "<max-depth>"
		end

		local valueType = type(value)
		if value == nil then
			return "nil"
		elseif valueType == "boolean" or valueType == "number" or valueType == "string" then
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
			local okPath, fullName = pcall(function()
				return value:GetFullName()
			end)
			return "Instance:" .. (if okPath then tostring(fullName) else tostring(value))
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
		if safeIsA(instance, "BasePart") then
			if lowered == "position" or lowered == "orientation" or lowered == "rotation" then
				return "CFrame"
			end
		elseif safeIsA(instance, "Model") or safeIsA(instance, "WorldModel") then
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

	local function readPropertyFingerprint(instance: Instance, propertyName: string): string?
		local lowered = string.lower(propertyName)
		if lowered == "attributes" or lowered == "attributereplicate" or lowered == "attributesreplicate" or lowered == "attributesserialize" then
			local okAttributes, attributes = pcall(function()
				return instance:GetAttributes()
			end)
			if not okAttributes then
				return nil
			end
			return stableValueString(attributes)
		end
		if lowered == "parent" then
			local pathSegments, pathOrdinals = pathSegmentsAndOrdinalsForInstance(instance)
			return stableValueString({
				pathSegments = pathSegments or {},
				pathOrdinals = pathOrdinals or {},
			})
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
			return nil
		end
		return stableValueString(value)
	end

	local function shouldRecordPropertyDirty(instance: Instance, propertyName: string): boolean
		local fingerprint = readPropertyFingerprint(instance, propertyName)
		if fingerprint == nil then
			return true
		end

		local cache = state.propertyFingerprintByInstance[instance]
		if cache == nil then
			cache = {}
			state.propertyFingerprintByInstance[instance] = cache
		end
		local key = propertyCacheKey(instance, propertyName)
		local previous = cache[key]
		cache[key] = fingerprint
		return previous == nil or previous ~= fingerprint
	end

	local function shouldRecordAttributeDirty(instance: Instance, attributeName: string): boolean
		local okAttribute, value = pcall(function()
			return instance:GetAttribute(attributeName)
		end)
		if not okAttribute then
			return true
		end

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
		local okSignal, signal = pcall(function()
			return (instance :: any).AttributeChanged
		end)
		if not okSignal or signal == nil then
			return nil
		end

		local okConnect, connection = pcall(function()
			return signal:Connect(function(attributeName: string)
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
		end)
		if okConnect and connection ~= nil then
			return connection :: RBXScriptConnection
		end
		return nil
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
			pcall(function()
				connection:Disconnect()
			end)
		end
	end

	local function disconnectInstanceTree(instance: Instance)
		local ok, descendants = pcall(function()
			return instance:GetDescendants()
		end)
		if ok then
			for _, descendant in ipairs(descendants) do
				disconnectInstance(descendant)
			end
		end
		disconnectInstance(instance)
	end

	local function connectInstance(instance: Instance, serviceName: string)
		if state.instanceConnections[instance] ~= nil or shouldIgnoreInstance(instance) then
			return
		end

		local connections: { RBXScriptConnection } = {}
		local okChanged, changedConnection = pcall(function()
			return instance.Changed:Connect(function(propertyName: any)
				local dirtyPropertyName = propertyName
				if safeIsA(instance, "ValueBase") then
					dirtyPropertyName = "Value"
				end
				if isRelevantInstanceProperty(instance, dirtyPropertyName) then
					local property = tostring(dirtyPropertyName)
					if not shouldRecordPropertyDirty(instance, property) then
						return
					end
					if not markDirectProperty(instance, serviceName, property) then
						markDirty(
							serviceName,
							true,
							changeDetailsForInstance(instance, "property", property, nil, "property changed")
						)
					end
				end
			end)
		end)
		if okChanged and changedConnection ~= nil then
			table.insert(connections, changedConnection :: RBXScriptConnection)
		end

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
		local ok, descendants = pcall(function()
			return service:GetDescendants()
		end)
		if not ok then
			return
		end
		for _, descendant in ipairs(descendants) do
			connectInstance(descendant, serviceName)
		end
	end

	local function ensureService(serviceName: string)
		if state.watchedServices[serviceName] == true then
			return
		end
		local service = game:GetService(serviceName)
		state.watchedServices[serviceName] = true
		state.serviceRoots[serviceName] = service

		local connections: { RBXScriptConnection } = {
			service.Changed:Connect(function(propertyName: string)
				local property = tostring(propertyName)
				if not shouldIgnoreRootProperty(service, serviceName, property) then
					if not shouldRecordPropertyDirty(service, property) then
						return
					end
					if not markDirectProperty(service, serviceName, property) then
						markDirty(
							serviceName,
							true,
							changeDetailsForInstance(service, "property", property, nil, "service property changed")
						)
					end
				end
			end),
			service.DescendantAdded:Connect(function(instance: Instance)
				if not shouldIgnoreInstance(instance) and (not state.onlyCodeMode or hasLuaSourceDescendant(instance)) then
					connectInstance(instance, serviceName)
					markDirty(
						serviceName,
						true,
						changeDetailsForInstance(instance, "added", nil, nil, "descendant added")
					)
				end
			end),
			service.DescendantRemoving:Connect(function(instance: Instance)
				if not shouldIgnoreInstance(instance) and (not state.onlyCodeMode or hasLuaSourceDescendant(instance)) then
					markDirty(
						serviceName,
						true,
						changeDetailsForInstance(instance, "removed", nil, nil, "descendant removing")
					)
				end
				disconnectInstanceTree(instance)
			end),
		}

		local rootAttributeConnection = connectAttributeChanged(service, serviceName)
		if rootAttributeConnection ~= nil then
			table.insert(connections, rootAttributeConnection)
		end
		state.rootConnections[serviceName] = connections
		connectExistingDescendants(service, serviceName)
	end

	local function ensureTracking(services: { string })
		if config.bridgeRole ~= "edit" then
			return
		end
		for _, serviceName in ipairs(services) do
			ensureService(serviceName)
		end
		if not state.started then
			local okItemChanged, itemChanged = pcall(function()
				return (game :: any).ItemChanged
			end)
			if okItemChanged and itemChanged ~= nil then
				local okConnect, connection = pcall(function()
					return itemChanged:Connect(function(instance: Instance, propertyName: any)
						if typeof(instance) == "Instance" and string.lower(tostring(propertyName or "")) == "tags" then
							markTagChange(instance, "Tags", true)
						end
					end)
				end)
				if okConnect and connection ~= nil then
					state.globalConnections[#state.globalConnections + 1] = connection
					state.itemChangedAvailable = true
				end
			end
			discoverTags(false)
			state.started = true
			task.spawn(function()
				while state.started do
					task.wait(0.5)
					if state.started then
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

	function api.setConflictResolution(value: string?)
		if value == "prompt" or value == "filesystem" or value == "studio" then
			state.conflictResolution = value
		end
	end

	function api.setOptions(rawOptions: any)
		if type(rawOptions) ~= "table" then
			return
		end
		if type(rawOptions.syncbackProperties) == "boolean" then
			state.syncbackProperties = rawOptions.syncbackProperties
		end
		if type(rawOptions.onlyCodeMode) == "boolean" then
			state.onlyCodeMode = rawOptions.onlyCodeMode
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
						state.propertyChangesByKey[key] = nil
					end
				end
				for key, change in pairs(state.changeLogByKey) do
					if change.service == serviceName and change.seq <= ackSeq then
						state.changeLogByKey[key] = nil
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
			if state.dirtySeqByService[change.service] ~= nil and state.fullSyncSeqByService[change.service] == nil then
				propertyChanges[#propertyChanges + 1] = change
			end
		end
		table.sort(propertyChanges, function(a, b)
			return a.seq < b.seq
		end)
		local changes = {}
		for _, change in pairs(state.changeLogByKey) do
			if state.dirtySeqByService[change.service] ~= nil then
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
		return {
			ok = true,
			tracking = state.started,
			role = config.bridgeRole,
			changeTrackerVersion = CHANGE_TRACKER_VERSION,
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
			trackedServices = #services,
			conflictResolution = state.conflictResolution,
			syncbackProperties = state.syncbackProperties,
			onlyCodeMode = state.onlyCodeMode,
		}
	end

	function api.getState(params: { [string]: any }): { [string]: any }
		local services = normalizeServices(params.services, allowedServices)
		if params.start ~= false then
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
		if not state.started then
			return
		end
		state.started = false
		for _, connections in pairs(state.rootConnections) do
			for _, connection in ipairs(connections) do
				pcall(function()
					connection:Disconnect()
				end)
			end
		end
		table.clear(state.rootConnections)
		for _, connection in ipairs(state.globalConnections) do
			pcall(function()
				connection:Disconnect()
			end)
		end
		table.clear(state.globalConnections)
		local connectedInstances = {}
		for instance in pairs(state.instanceConnections) do
			connectedInstances[#connectedInstances + 1] = instance
		end
		for _, instance in ipairs(connectedInstances) do
			disconnectInstance(instance)
		end
		for _, connections in pairs(state.tagConnections) do
			for _, connection in ipairs(connections) do
				pcall(function()
					connection:Disconnect()
				end)
			end
		end
		table.clear(state.tagConnections)
		table.clear(state.taggedInstancesByTag)
		pcall(function()
			state.changeEvent:Destroy()
		end)
	end

	return api
end

return BridgeStudioChanges
