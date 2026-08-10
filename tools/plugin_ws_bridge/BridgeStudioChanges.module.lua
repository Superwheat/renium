
local BridgeStudioChanges = {}
local BridgeValueCodec = require(script.Parent.BridgeValueCodec)
local CHANGE_TRACKER_VERSION = 4
local CollectionService = game:GetService("CollectionService")
local Workspace = game:GetService("Workspace")
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
	valueCaptured: boolean?,
	value: any,
	instance: Instance?,
	journalValueCaptured: boolean?,
	journalValue: any,
}
type ExpectedValue = {
	value: any,
}
type ExpectedValueQueue = { ExpectedValue }
type ExpectedInstanceEvent = {
	active: boolean,
	fingerprint: string?,
	matchParent: boolean?,
	parent: Instance?,
}
type ExpectedInstanceEventQueue = { ExpectedInstanceEvent }
type ExpectedInstanceEvents = { [Instance]: { [string]: ExpectedInstanceEventQueue } }
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

local ATTRIBUTE_EVENT_PROPERTIES: { [string]: boolean } = {
	attributes = true,
	attributereplicate = true,
	attributesreplicate = true,
	attributesserialize = true,
}

type State = {
	started: boolean,
	seq: number,
	dirtySeqByService: DirtySeqMap,
	mutationSeqByService: DirtySeqMap,
	fullSyncSeqByService: DirtySeqMap,
	propertyChangesByKey: { [string]: DirectPropertyChange },
	changeLogByKey: { [string]: StudioChangeLog },
	propertyFingerprintByInstance: PropertyFingerprintMap,
	ordinalCacheByParent: { [Instance]: { [Instance]: number } },
	lastParentByInstance: { [Instance]: Instance? },
	watchedServices: { [string]: boolean },
	serviceRoots: { [string]: Instance },
	serviceNameByRoot: { [Instance]: string },
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
	expectedProperties: { [string]: ExpectedValueQueue },
	expectedAttributes: { [string]: ExpectedValueQueue },
	expectedStructuralByInstance: ExpectedInstanceEvents,
	expectedInstanceProperties: ExpectedInstanceEvents,
	expectedInstanceAttributes: ExpectedInstanceEvents,
	expectedTags: ExpectedInstanceEvents,
	expectedGeneration: number,
	luaSourceDescendantCounts: { [Instance]: number },
	changeJournal: any?,
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
		mutationSeqByService = {},
		fullSyncSeqByService = {},
		propertyChangesByKey = {},
		changeLogByKey = {},
		propertyFingerprintByInstance = setmetatable({}, { __mode = "k" }) :: any,
		ordinalCacheByParent = setmetatable({}, { __mode = "k" }) :: any,
		lastParentByInstance = setmetatable({}, { __mode = "k" }) :: any,
		watchedServices = {},
		serviceRoots = {},
		serviceNameByRoot = {},
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
		expectedProperties = {},
		expectedAttributes = {},
		expectedStructuralByInstance = setmetatable({}, { __mode = "k" }) :: any,
		expectedInstanceProperties = setmetatable({}, { __mode = "k" }) :: any,
		expectedInstanceAttributes = setmetatable({}, { __mode = "k" }) :: any,
		expectedTags = setmetatable({}, { __mode = "k" }) :: any,
		expectedGeneration = 0,
		luaSourceDescendantCounts = setmetatable({}, { __mode = "k" }) :: any,
		changeJournal = nil,
	}

	local api = {}
	local luaSourceClasses = config.LUA_SOURCE_CLASS

	local function persistPendingServices()
		local services = {}
		for serviceName in pairs(state.dirtySeqByService) do
			services[#services + 1] = serviceName
		end
		table.sort(services)
		config.savePendingStudioChanges(services)
	end

	local pending = config.loadPendingStudioChanges()
	if type(pending) == "table" then
		for _, serviceName in pending do
			if type(serviceName) == "string" and allowedServices[serviceName] then
				state.seq += 1
				state.dirtySeqByService[serviceName] = state.seq
				state.mutationSeqByService[serviceName] = state.seq
				state.fullSyncSeqByService[serviceName] = state.seq
			end
		end
	end

	local function isSuppressed(): boolean
		return state.suppressDepth > 0 or os.clock() < state.suppressUntil
	end

	local function expectedPathKey(
		serviceName: string,
		pathSegments: { string }?,
		pathOrdinals: { number }?,
		name: string?
	): string
		return serviceName
			.. "\0"
			.. structuredPathKey(pathSegments, pathOrdinals)
			.. "\0"
			.. tostring(name or "")
	end

	local function expectedValuesEqual(left: any, right: any): boolean
		if type(left) == "number" or type(right) == "number" then
			return BridgeValueCodec.numbersEqual(left, right)
		end
		if type(left) ~= type(right) then
			return false
		end
		if type(left) ~= "table" then
			return left == right
		end
		for key, value in pairs(left) do
			if not expectedValuesEqual(value, right[key]) then
				return false
			end
		end
		for key in pairs(right) do
			if left[key] == nil then
				return false
			end
		end
		return true
	end

	local function isExpectedChange(serviceName: string, details: StudioChangeDetails?): boolean
		if not isSuppressed() or type(details) ~= "table" then
			return false
		end
		local action = tostring(details.action or "")
		local keyName = if action == "attribute" then details.attribute else details.property
		local key = expectedPathKey(serviceName, details.pathSegments, details.pathOrdinals, keyName)
		if details.valueCaptured ~= true then
			return false
		end
		local expectedValues = if action == "attribute"
			then state.expectedAttributes
			else if action == "property" then state.expectedProperties else nil
		if expectedValues == nil then
			return false
		end
		local queue = expectedValues[key]
		if queue == nil then
			return false
		end
		for index, expected in ipairs(queue) do
			if expectedValuesEqual(expected.value, details.value) then
				table.remove(queue, index)
				if #queue == 0 then
					expectedValues[key] = nil
				end
				return true
			end
		end
		return false
	end

	local stableValueString: (any, number?) -> string
	local expectedPropertyFingerprint: (Instance, string, any) -> string?

	local function consumeExpectedInstanceEvent(
		target: ExpectedInstanceEvents,
		instance: Instance,
		action: string,
		fingerprint: string?,
		parent: Instance?
	): boolean
		if not isSuppressed() then
			return false
		end
		local expected = target[instance]
		local queue = if expected ~= nil then expected[action] else nil
		if queue == nil then
			return false
		end
		local index = 1
		while index <= #queue do
			local event = queue[index]
			if not event.active then
				table.remove(queue, index)
			elseif
				(event.fingerprint == nil or event.fingerprint == fingerprint)
				and (not event.matchParent or event.parent == parent)
			then
				event.active = false
				table.remove(queue, index)
				if #queue == 0 then
					expected[action] = nil
					if not next(expected) then
						target[instance] = nil
					end
				end
				return true
			else
				index += 1
			end
		end
		if #queue == 0 then
			expected[action] = nil
			if not next(expected) then
				target[instance] = nil
			end
		end
		return false
	end

	local function expectInstanceEvent(
		target: ExpectedInstanceEvents,
		instance: Instance,
		action: string,
		fingerprint: string?,
		matchParent: boolean?,
		parent: Instance?
	): ExpectedInstanceEvent
		local expected = target[instance]
		if expected == nil then
			expected = {}
			target[instance] = expected
		end
		local queue = expected[action]
		if queue == nil then
			queue = {}
			expected[action] = queue
		end
		local event = {
			active = true,
			fingerprint = fingerprint,
			matchParent = matchParent,
			parent = parent,
		}
		queue[#queue + 1] = event
		return event
	end

	function api.expectParentChange(instance: Instance, nextParent: Instance?)
		local instances = { instance }
		local tokens = {}
		for _, descendant in ipairs(instance:GetDescendants()) do
			instances[#instances + 1] = descendant
		end
		for _, target in ipairs(instances) do
			if instance.Parent ~= nil then
				tokens[#tokens + 1] =
					expectInstanceEvent(state.expectedStructuralByInstance, target, "removed", nil, true, target.Parent)
			end
			if nextParent ~= nil then
				local expectedParent = if target == instance then nextParent else target.Parent
				tokens[#tokens + 1] =
					expectInstanceEvent(state.expectedStructuralByInstance, target, "added", nil, true, expectedParent)
			end
		end
		if state.instanceConnections[instance] ~= nil then
			tokens[#tokens + 1] =
				expectInstanceEvent(state.expectedInstanceProperties, instance, "parent", nil, false, nil)
		end
		return { tokens = tokens }
	end

	function api.expectPropertyEvent(instance: Instance, propertyName: string, value: any)
		return expectInstanceEvent(
			state.expectedInstanceProperties,
			instance,
			string.lower(propertyName),
			expectedPropertyFingerprint(instance, propertyName, value),
			false,
			nil
		)
	end

	function api.expectAttributeEvent(instance: Instance, attributeName: string, value: any)
		return expectInstanceEvent(
			state.expectedInstanceAttributes,
			instance,
			attributeName,
			stableValueString(value),
			false,
			nil
		)
	end

	function api.expectTagChange(instance: Instance, tag: string, added: boolean)
		local action = (if added then "added:" else "removed:") .. tag
		return {
			tokens = {
				expectInstanceEvent(state.expectedTags, instance, action, nil, false, nil),
				expectInstanceEvent(state.expectedTags, instance, "property", nil, false, nil),
			},
		}
	end

	function api.cancelExpectedEvent(token: any)
		if type(token) ~= "table" then
			return
		end
		if type(token.tokens) == "table" then
			for _, child in ipairs(token.tokens) do
				api.cancelExpectedEvent(child)
			end
			return
		end
		token.active = false
	end

	function api.serviceGeneration(serviceName: string): number
		return state.mutationSeqByService[serviceName] or 0
	end

	function api.beginChangeJournal(transactionId: string, services: { string })
		if state.changeJournal ~= nil then
			error("Another Studio change journal is already active")
		end
		local included = {}
		for _, serviceName in ipairs(services) do
			included[serviceName] = true
		end
		state.changeJournal = {
			id = transactionId,
			services = included,
			records = {},
			recordsByInstance = {},
		}
	end

	function api.drainChangeJournal(transactionId: string): { any }
		local journal = state.changeJournal
		if journal == nil then
			return {}
		end
		if journal.id ~= transactionId then
			error("Studio change journal does not match the active transaction")
		end
		local records = journal.records
		journal.records = {}
		journal.recordsByInstance = {}
		return records
	end

	function api.finishChangeJournal(transactionId: string): { any }
		local records = api.drainChangeJournal(transactionId)
		state.changeJournal = nil
		return records
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
		if duration <= 0 then
			return hasDirtyServices(services)
		end
		if hasDirtyServices(services) then
			return true
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

	local function recordJournalChange(serviceName: string, details: StudioChangeDetails?)
		local journal = state.changeJournal
		local instance = if details ~= nil then details.instance else nil
		if
			journal == nil
			or not journal.services[serviceName]
			or instance == nil
			or typeof(instance) ~= "Instance"
		then
			return
		end
		local record = journal.recordsByInstance[instance]
		if record == nil then
			record = {
				instance = instance,
				service = serviceName,
				properties = {},
				attributes = {},
			}
			journal.recordsByInstance[instance] = record
			journal.records[#journal.records + 1] = record
		end
		if details.pathSegments ~= nil and #details.pathSegments > 0 then
			record.pathSegments = table.clone(details.pathSegments)
			record.pathOrdinals = table.clone(details.pathOrdinals or {})
		end
		local action = tostring(details.action or "")
		if action == "property" and details.property ~= nil then
			record.properties[details.property] = {
				captured = details.journalValueCaptured == true,
				value = details.journalValue,
			}
		elseif action == "attribute" and details.attribute ~= nil then
			record.attributes[details.attribute] = {
				captured = details.journalValueCaptured == true,
				value = details.journalValue,
			}
		elseif action == "tag" then
			record.tagsChanged = true
		elseif action == "added" or action == "removed" then
			record.structural = true
		end
	end

	local function markDirty(serviceName: string?, requiresFullSync: boolean?, details: StudioChangeDetails?)
		if serviceName == nil or not allowedServices[serviceName] then
			return
		end
		if isExpectedChange(serviceName, details) then
			return
		end
		recordJournalChange(serviceName, details)
		local wasDirty = state.dirtySeqByService[serviceName] ~= nil
		state.seq += 1
		state.dirtySeqByService[serviceName] = state.seq
		state.mutationSeqByService[serviceName] = state.seq
		local isFullSync = requiresFullSync ~= false
		if isFullSync then
			state.fullSyncSeqByService[serviceName] = state.seq
			clearPropertyChangesForService(serviceName)
		end
		recordChange(serviceName, state.seq, isFullSync, details)
		if not wasDirty then
			persistPendingServices()
		end
		signalChange()
	end

	local function directPropertyKey(
		serviceName: string,
		pathSegments: { string },
		pathOrdinals: { number },
		scope: string,
		propertyName: string
	): string
		return serviceName
			.. "\0"
			.. structuredPathKey(pathSegments, pathOrdinals)
			.. "\0"
			.. scope
			.. "\0"
			.. propertyName
	end

	local function directChangeLogKey(
		serviceName: string,
		pathSegments: { string },
		pathOrdinals: { number },
		action: string,
		name: string
	): string
		return serviceName
			.. "\0"
			.. action
			.. "\0"
			.. structuredPathKey(pathSegments, pathOrdinals)
			.. "\0"
			.. name
	end

	local function removeQueuedDirectChange(
		serviceName: string,
		pathSegments: { string },
		pathOrdinals: { number },
		scope: string,
		name: string
	)
		local key = directPropertyKey(serviceName, pathSegments, pathOrdinals, scope, name)
		local previous = state.propertyChangesByKey[key]
		if previous ~= nil then
			state.directPropertyBytes = math.max(0, state.directPropertyBytes - (previous.estimatedBytes or 0))
			state.directPropertyCount = math.max(0, state.directPropertyCount - 1)
			state.propertyChangesByKey[key] = nil
		end
		local logKey = directChangeLogKey(serviceName, pathSegments, pathOrdinals, scope, name)
		if state.changeLogByKey[logKey] ~= nil then
			state.changeLogByKey[logKey] = nil
			state.changeLogCountByService[serviceName] =
				math.max(0, (state.changeLogCountByService[serviceName] or 0) - 1)
		end
		if state.fullSyncSeqByService[serviceName] ~= nil then
			return
		end
		for _, change in pairs(state.propertyChangesByKey) do
			if change.service == serviceName then
				return
			end
		end
		for _, change in pairs(state.changeLogByKey) do
			if change.service == serviceName then
				return
			end
		end
		if state.dirtySeqByService[serviceName] ~= nil then
			state.dirtySeqByService[serviceName] = nil
			persistPendingServices()
		end
	end

	local function canTrackDirectProperty(propertyName: string): boolean
		return not FULL_SYNC_PROPERTIES[string.lower(propertyName)]
	end

	local function encodeDirectValue(value: any): (boolean, any)
		local valueType = type(value)
		if value == nil then
			return true, nil
		elseif valueType == "boolean" or valueType == "string" then
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
			segments[#segments + 1] = current.Name
			ordinals[#ordinals + 1] = ordinal
			current = parent
		end
		for left = 1, math.floor(#segments / 2) do
			local right = #segments - left + 1
			segments[left], segments[right] = segments[right], segments[left]
			ordinals[left], ordinals[right] = ordinals[right], ordinals[left]
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
			instance = instance,
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

	local function markDirectValue(
		instance: Instance,
		serviceName: string,
		scope: string,
		name: string,
		capturedOk: boolean?,
		capturedValue: any,
		journalValueCaptured: boolean?,
		journalValue: any
	): boolean
		local pathSegments, pathOrdinals = pathSegmentsAndOrdinalsForInstance(instance)
		if pathSegments == nil or pathOrdinals == nil or #pathSegments == 0 or pathSegments[1] ~= serviceName then
			return false
		end
		local okValue, value
		if capturedOk ~= nil then
			okValue = capturedOk
			value = capturedValue
		else
			if scope == "attribute" then
				okValue, value = encodeDirectValue(instance:GetAttribute(name))
			else
				okValue, value = encodeDirectPropertyValue(instance, name)
			end
		end
		if
			isExpectedChange(serviceName, {
				action = scope,
				property = if scope == "property" then name else nil,
				attribute = if scope == "attribute" then name else nil,
				pathSegments = pathSegments,
				pathOrdinals = pathOrdinals,
				valueCaptured = okValue,
				value = value,
			})
		then
			removeQueuedDirectChange(serviceName, pathSegments, pathOrdinals, scope, name)
			return true
		end
		if state.fullSyncSeqByService[serviceName] ~= nil then
			local details = changeDetailsForInstance(
				instance,
				scope,
				if scope == "property" then name else nil,
				if scope == "attribute" then name else nil,
				scope .. " changed while a Studio pull was pending"
			)
			details.journalValueCaptured = journalValueCaptured
			details.journalValue = journalValue
			markDirty(
				serviceName,
				true,
				details
			)
			return true
		end
		if scope == "property" and not canTrackDirectProperty(name) then
			return false
		end
		if not okValue then
			return false
		end
		local key = directPropertyKey(serviceName, pathSegments, pathOrdinals, scope, name)
		local previous = state.propertyChangesByKey[key]
		local previousBytes = if previous ~= nil then previous.estimatedBytes else 0
		local estimatedBytes = #key + estimatedValueBytes(value) + 128
		local nextCount = state.directPropertyCount + (if previous == nil then 1 else 0)
		local nextBytes = state.directPropertyBytes - previousBytes + estimatedBytes
		recordJournalChange(serviceName, {
			instance = instance,
			action = scope,
			property = if scope == "property" then name else nil,
			attribute = if scope == "attribute" then name else nil,
			pathSegments = pathSegments,
			pathOrdinals = pathOrdinals,
			journalValueCaptured = journalValueCaptured,
			journalValue = journalValue,
			value = nil,
		})
		if nextCount > MAX_DIRECT_PROPERTY_CHANGES or nextBytes > MAX_DIRECT_PROPERTY_BYTES then
			markDirty(serviceName, true, {
				action = "fullSync",
				reason = "direct property retention limit reached",
				path = serviceName,
				fullSync = true,
			})
			return true
		end

		local wasDirty = state.dirtySeqByService[serviceName] ~= nil
		state.seq += 1
		state.dirtySeqByService[serviceName] = state.seq
		state.mutationSeqByService[serviceName] = state.seq
		recordChange(serviceName, state.seq, false, {
			action = scope,
			reason = scope .. " changed",
			className = instance.ClassName,
			pathSegments = pathSegments,
			pathOrdinals = pathOrdinals,
			path = pathToString(pathSegments),
			property = if scope == "property" then name else nil,
			attribute = if scope == "attribute" then name else nil,
			direct = true,
			fullSync = false,
		})
		state.propertyChangesByKey[key] = {
			service = serviceName,
			className = instance.ClassName,
			pathSegments = pathSegments,
			pathOrdinals = pathOrdinals,
			scope = scope,
			property = name,
			value = value,
			seq = state.seq,
			estimatedBytes = estimatedBytes,
		}
		state.directPropertyCount = nextCount
		state.directPropertyBytes = nextBytes
		if not wasDirty then
			persistPendingServices()
		end
		signalChange()
		return true
	end

	local function markDirectProperty(
		instance: Instance,
		serviceName: string,
		propertyName: string,
		capturedOk: boolean?,
		capturedValue: any,
		journalValueCaptured: boolean?,
		journalValue: any
	): boolean
		return markDirectValue(
			instance,
			serviceName,
			"property",
			propertyName,
			capturedOk,
			capturedValue,
			journalValueCaptured,
			journalValue
		)
	end

	local function shouldIgnoreInstance(instance: Instance): boolean
		if config.shouldIgnoreInstance(instance) then
			return true
		end
		local currentCamera = Workspace.CurrentCamera
		if currentCamera == nil then
			return false
		end
		if instance == currentCamera then
			return true
		end
		return instance:IsDescendantOf(currentCamera)
	end

	local function isLuaSourceInstance(instance: Instance): boolean
		return luaSourceClasses[instance.ClassName] == true
	end

	local function hasLuaSourceDescendant(instance: Instance): boolean
		local count = state.luaSourceDescendantCounts[instance]
		if count then
			return count > 0
		end
		count = if isLuaSourceInstance(instance) then 1 else 0
		for _, child in ipairs(instance:GetChildren()) do
			if hasLuaSourceDescendant(child) then
				count += state.luaSourceDescendantCounts[child] or 0
			end
		end
		state.luaSourceDescendantCounts[instance] = count
		return count > 0
	end

	local function adjustLuaSourceAncestors(instance: Instance, delta: number)
		local current = instance
		while current ~= nil do
			state.luaSourceDescendantCounts[current] =
				math.max(0, (state.luaSourceDescendantCounts[current] or 0) + delta)
			current = current.Parent
		end
	end

	local function rebuildLuaSourceCounts(root: Instance): { Instance }
		local descendants = root:GetDescendants()
		state.luaSourceDescendantCounts[root] = if isLuaSourceInstance(root) then 1 else 0
		for _, instance in ipairs(descendants) do
			state.luaSourceDescendantCounts[instance] = if isLuaSourceInstance(instance) then 1 else 0
		end
		for index = #descendants, 1, -1 do
			local instance = descendants[index]
			local parent = instance.Parent
			if parent ~= nil then
				state.luaSourceDescendantCounts[parent] = (state.luaSourceDescendantCounts[parent] or 0)
					+ state.luaSourceDescendantCounts[instance]
			end
		end
		return descendants
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
		local current: Instance? = instance
		while current ~= nil and current ~= game do
			local serviceName = state.serviceNameByRoot[current]
			if serviceName ~= nil then
				return serviceName
			end
			current = current.Parent
		end
		return nil
	end

	local function tagChangeRelevant(instance: Instance): boolean
		return not state.onlyCodeMode or hasLuaSourceDescendant(instance)
	end

	local function markTagChange(instance: Instance, tag: string, added: boolean)
		if isSuppressed() then
			if tag == "Tags" then
				if consumeExpectedInstanceEvent(state.expectedTags, instance, "property", nil, nil) then
					return
				end
			else
				local action = (if added then "added:" else "removed:") .. tag
				if consumeExpectedInstanceEvent(state.expectedTags, instance, action, nil, nil) then
					return
				end
			end
		end
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

	stableValueString = function(value: any, depth: number?): string
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
		if ATTRIBUTE_EVENT_PROPERTIES[lowered] then
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

	expectedPropertyFingerprint = function(instance: Instance, propertyName: string, value: any): string?
		if string.lower(propertyReadNameForEvent(instance, propertyName)) ~= string.lower(propertyName) then
			return nil
		end
		return stableValueString(value)
	end

	local function readPropertyFingerprint(
		instance: Instance,
		propertyName: string
	): (string?, boolean, any, boolean, any)
		local lowered = string.lower(propertyName)
		if ATTRIBUTE_EVENT_PROPERTIES[lowered] then
			local attributes = instance:GetAttributes()
			return stableValueString(attributes), false, nil, true, attributes
		end
		if lowered == "parent" then
			local pathSegments, pathOrdinals = pathSegmentsAndOrdinalsForInstance(instance)
			return stableValueString({
				pathSegments = pathSegments or {},
				pathOrdinals = pathOrdinals or {},
			}), false, nil, true, instance.Parent
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
			return nil, false, nil, false, nil
		end
		local directOk, directValue = encodeDirectValue(value)
		return stableValueString(value), directOk, directValue, true, value
	end

	local function shouldRecordPropertyDirty(
		instance: Instance,
		propertyName: string
	): (boolean, boolean, any, string?, boolean, any)
		local fingerprint, directOk, directValue, valueCaptured, value =
			readPropertyFingerprint(instance, propertyName)
		if fingerprint == nil then
			return true, false, nil, nil, valueCaptured, value
		end

		local cache = state.propertyFingerprintByInstance[instance]
		if cache == nil then
			cache = {}
			state.propertyFingerprintByInstance[instance] = cache
		end
		local key = propertyCacheKey(instance, propertyName)
		local previous = cache[key]
		cache[key] = fingerprint
		return previous == nil or previous ~= fingerprint, directOk, directValue, fingerprint, valueCaptured, value
	end

	local function shouldRecordAttributeDirty(
		instance: Instance,
		attributeName: string
	): (boolean, boolean, any, string, boolean, any)
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
		local directOk, directValue = encodeDirectValue(value)
		return previous == nil or previous ~= fingerprint, directOk, directValue, fingerprint, true, value
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
				local shouldRecord, directOk, directValue, fingerprint, valueCaptured, value =
					shouldRecordAttributeDirty(instance, attribute)
				if shouldRecord then
					if
						consumeExpectedInstanceEvent(
							state.expectedInstanceAttributes,
							instance,
							attribute,
							fingerprint,
							nil
						)
					then
						return
					end
					if
						not markDirectValue(
							instance,
							serviceName,
							"attribute",
							attribute,
							directOk,
							directValue,
							valueCaptured,
							value
						)
					then
						local details =
							changeDetailsForInstance(instance, "attribute", nil, attribute, "attribute changed")
						details.valueCaptured = directOk
						details.value = directValue
						details.journalValueCaptured = valueCaptured
						details.journalValue = value
						markDirty(serviceName, true, details)
					end
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
		state.lastParentByInstance[instance] = nil
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
		state.lastParentByInstance[instance] = instance.Parent
		local changedConnection = instance.Changed:Connect(function(propertyName: any)
				local dirtyPropertyName = if instance:IsA("ValueBase") then "Value" else propertyName
				if isRelevantInstanceProperty(instance, dirtyPropertyName) then
					local property = tostring(dirtyPropertyName)
					local lowered = string.lower(property)
					if lowered == "name" then
						invalidateSiblingOrdinals(instance.Parent)
					end
					local shouldRecord, directOk, directValue, fingerprint, valueCaptured, value =
						shouldRecordPropertyDirty(instance, property)
					if not shouldRecord then
						return
					end
					if
						consumeExpectedInstanceEvent(
							state.expectedInstanceProperties,
							instance,
							lowered,
							fingerprint,
							nil
						)
					then
						return
					end
					if
						not markDirectProperty(
							instance,
							serviceName,
							property,
							directOk,
							directValue,
							valueCaptured,
							value
						)
					then
						local details =
							changeDetailsForInstance(instance, "property", property, nil, "property changed")
						details.journalValueCaptured = valueCaptured
						details.journalValue = value
						markDirty(serviceName, true, details)
					end
				end
			end)
		table.insert(connections, changedConnection)
		table.insert(connections, instance.AncestryChanged:Connect(function(_, parent: Instance?)
			local previousParent = state.lastParentByInstance[instance]
			invalidateSiblingOrdinals(previousParent)
			invalidateSiblingOrdinals(parent)
			state.lastParentByInstance[instance] = parent
		end))

		local attributeConnection = connectAttributeChanged(instance, serviceName)
		if attributeConnection ~= nil then
			table.insert(connections, attributeConnection)
		end

		if #connections > 0 then
			state.instanceConnections[instance] = connections
			state.connectedInstanceCount += 1
		end
	end

	local function connectExistingDescendants(descendants: { Instance }, serviceName: string)
		for _, descendant in ipairs(descendants) do
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
		state.serviceNameByRoot[service] = serviceName
		local descendants = rebuildLuaSourceCounts(service)

		local connections: { RBXScriptConnection } = {
			service.Changed:Connect(function(propertyName: string)
				local property = tostring(propertyName)
				if not shouldIgnoreRootProperty(service, serviceName, property) then
					local shouldRecord, directOk, directValue, fingerprint, valueCaptured, value =
						shouldRecordPropertyDirty(service, property)
					if not shouldRecord then
						return
					end
					if
						consumeExpectedInstanceEvent(
							state.expectedInstanceProperties,
							service,
							string.lower(property),
							fingerprint,
							nil
						)
					then
						return
					end
					if
						not markDirectProperty(
							service,
							serviceName,
							property,
							directOk,
							directValue,
							valueCaptured,
							value
						)
					then
						local details =
							changeDetailsForInstance(service, "property", property, nil, "service property changed")
						details.journalValueCaptured = valueCaptured
						details.journalValue = value
						markDirty(serviceName, true, details)
					end
				end
			end),
			service.DescendantAdded:Connect(function(instance: Instance)
				local expected =
					consumeExpectedInstanceEvent(state.expectedStructuralByInstance, instance, "added", nil, instance.Parent)
				if isLuaSourceInstance(instance) then
					adjustLuaSourceAncestors(instance, 1)
				elseif state.luaSourceDescendantCounts[instance] == nil then
					state.luaSourceDescendantCounts[instance] = 0
				end
				invalidateSiblingOrdinals(instance.Parent)
				if not shouldIgnoreInstance(instance) and (not state.onlyCodeMode or hasLuaSourceDescendant(instance)) then
					connectInstance(instance, serviceName)
					reconcileAncestorConnections(instance, service, serviceName)
					if not expected then
						markDirty(
							serviceName,
							true,
							changeDetailsForInstance(instance, "added", nil, nil, "descendant added")
						)
					end
				end
			end),
			service.DescendantRemoving:Connect(function(instance: Instance)
				local expected = consumeExpectedInstanceEvent(
					state.expectedStructuralByInstance,
					instance,
					"removed",
					nil,
					instance.Parent
				)
				local wasCodeRelevant = not state.onlyCodeMode or hasLuaSourceDescendant(instance)
				if isLuaSourceInstance(instance) then
					adjustLuaSourceAncestors(instance, -1)
				end
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
				if not expected and not shouldIgnoreInstance(instance) and wasCodeRelevant then
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
		connectExistingDescendants(descendants, serviceName)
	end

	local function unwatchService(serviceName: string, preservePending: boolean?)
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
		state.serviceNameByRoot[service] = nil
		if not preservePending then
			state.dirtySeqByService[serviceName] = nil
			state.fullSyncSeqByService[serviceName] = nil
			clearPropertyChangesForService(serviceName)
			clearChangeLogsForService(serviceName)
			persistPendingServices()
		end
	end

	local function stopTracking()
		state.started = false
		state.tagPollToken += 1
		local watched = {}
		for serviceName in pairs(state.watchedServices) do
			table.insert(watched, serviceName)
		end
		for _, serviceName in ipairs(watched) do
			unwatchService(serviceName, true)
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

	function api.suppress(seconds: number?)
		local duration = tonumber(seconds) or 0.2
		if duration <= 0 then
			return
		end
		state.suppressUntil = math.max(state.suppressUntil, os.clock() + duration)
	end

	local function addExpectedMutation(raw: any)
		if type(raw) ~= "table" then
			return
		end
		local function addExpected(
			target: { [string]: ExpectedValueQueue },
			key: string,
			value: any
		)
			local queue = target[key]
			if queue == nil then
				queue = {}
				target[key] = queue
			end
			queue[#queue + 1] = { value = value }
		end
		for _, change in ipairs(raw.sourceChanges or {}) do
			local serviceName = tostring(change.service or "")
			if allowedServices[serviceName] then
				addExpected(
					state.expectedProperties,
					expectedPathKey(serviceName, change.pathSegments, change.pathOrdinals, "Source"),
					change.source
				)
			end
		end
		for _, change in ipairs(raw.propertyChanges or {}) do
			local serviceName = tostring(change.service or "")
			if allowedServices[serviceName] then
				for propertyName, value in pairs(change.properties or {}) do
					addExpected(
						state.expectedProperties,
						expectedPathKey(
							serviceName,
							change.pathSegments,
							change.pathOrdinals,
							tostring(propertyName)
						),
						value
					)
				end
				for attributeName, value in pairs(change.attributes or {}) do
					addExpected(
						state.expectedAttributes,
						expectedPathKey(
							serviceName,
							change.pathSegments,
							change.pathOrdinals,
							tostring(attributeName)
						),
						value
					)
				end
				for _, attributeName in ipairs(change.deletedAttributes or {}) do
					addExpected(
						state.expectedAttributes,
						expectedPathKey(
							serviceName,
							change.pathSegments,
							change.pathOrdinals,
							tostring(attributeName)
						),
						nil
					)
				end
			end
		end
	end

	local function clearExpectedEvents()
		table.clear(state.expectedProperties)
		table.clear(state.expectedAttributes)
		table.clear(state.expectedStructuralByInstance)
		table.clear(state.expectedInstanceProperties)
		table.clear(state.expectedInstanceAttributes)
		table.clear(state.expectedTags)
	end

	function api.beginSuppress(seconds: number?, expectation: any)
		if state.suppressDepth == 0 then
			state.expectedGeneration += 1
			clearExpectedEvents()
		end
		state.suppressDepth += 1
		addExpectedMutation(expectation)
		local duration = tonumber(seconds)
		if duration and duration > 0 then
			api.suppress(duration)
		end
	end

	function api.endSuppress(settleSeconds: number?)
		state.suppressDepth = math.max(0, state.suppressDepth - 1)
		if state.suppressDepth == 0 then
			local duration = tonumber(settleSeconds) or 0.2
			if duration > 0 then
				api.suppress(duration)
			end
			local generation = state.expectedGeneration
			local function clearWhenSettled()
				if state.suppressDepth == 0 and state.expectedGeneration == generation then
					clearExpectedEvents()
				end
			end
			if duration > 0 then
				task.delay(duration, clearWhenSettled)
			else
				task.spawn(clearWhenSettled)
			end
		end
	end

	local function applyStateParams(params: { [string]: any }, services: { string })
		local suppressSeconds = tonumber(params.suppressSeconds)
		if suppressSeconds and suppressSeconds > 0 then
			api.suppress(suppressSeconds)
		end

		local ackSeq = tonumber(params.ackSeq)
		if ackSeq then
			if type(params.runtimeId) ~= "string" or params.runtimeId ~= config.bridgeRuntimeId then
				error("Studio change acknowledgment runtime does not match the active plugin runtime")
			end
			local requested = {}
			for _, serviceName in ipairs(services) do
				requested[serviceName] = true
				local dirtySeq = state.dirtySeqByService[serviceName]
				if dirtySeq ~= nil and dirtySeq <= ackSeq then
					state.dirtySeqByService[serviceName] = nil
				end
				local fullSyncSeq = state.fullSyncSeqByService[serviceName]
				if fullSyncSeq ~= nil and fullSyncSeq <= ackSeq then
					state.fullSyncSeqByService[serviceName] = nil
				end
			end
			for key, change in pairs(state.propertyChangesByKey) do
				if requested[change.service] and change.seq <= ackSeq then
					state.directPropertyBytes =
						math.max(0, state.directPropertyBytes - change.estimatedBytes)
					state.directPropertyCount = math.max(0, state.directPropertyCount - 1)
					state.propertyChangesByKey[key] = nil
				end
			end
			for key, change in pairs(state.changeLogByKey) do
				if requested[change.service] and change.seq <= ackSeq then
					state.changeLogByKey[key] = nil
					state.changeLogCountByService[change.service] =
						math.max(0, (state.changeLogCountByService[change.service] or 0) - 1)
				end
			end
			persistPendingServices()
		end
		if params.clearPending == true or params.reset == true then
			for _, serviceName in ipairs(services) do
				state.dirtySeqByService[serviceName] = nil
				state.fullSyncSeqByService[serviceName] = nil
				clearPropertyChangesForService(serviceName)
				clearChangeLogsForService(serviceName)
			end
			persistPendingServices()
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
			if params.replaceServices == true or params.reset == true then
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
		if
			waitSeconds
			and waitSeconds > 0
			and params.clearPending ~= true
			and params.reset ~= true
			and params.ackSeq == nil
		then
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

	function api.pendingChangeCount(): number
		local count = 0
		for _, change in pairs(state.changeLogByKey) do
			if state.dirtySeqByService[change.service] ~= nil then
				count += 1
			end
		end
		if count > 0 then
			return count
		end
		for serviceName in pairs(allowedServices) do
			if state.dirtySeqByService[serviceName] ~= nil then
				count += 1
			end
		end
		return count
	end

	function api.onChanged(callback): RBXScriptConnection
		return state.changeEvent.Event:Connect(callback)
	end

	function api.stop()
		stopTracking()
		state.changeJournal = nil
		state.changeEvent:Destroy()
	end

	return api
end

return BridgeStudioChanges
