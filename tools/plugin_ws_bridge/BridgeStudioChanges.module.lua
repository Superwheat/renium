local BridgeStudioChanges = {}
local BridgeScriptDocuments = require(script.Parent.BridgeScriptDocuments)
local BridgeValueCodec = require(script.Parent.BridgeValueCodec)
local BridgeValueEquality = require(script.Parent.BridgeValueEquality)
local BridgeContent = require(script.Parent.BridgeContent)
local BridgeIdentity = require(script.Parent.BridgeIdentity)
local RbxDomModule = require(script.Parent.RbxDom)
local CHANGE_TRACKER_VERSION = 4
local CollectionService = game:GetService("CollectionService")
local Workspace = game:GetService("Workspace")
local MAX_CHANGE_LOGS_PER_SERVICE = 1024
local MAX_DIRECT_PROPERTY_CHANGES = 2048
local MAX_DIRECT_PROPERTY_BYTES = 8 * 1024 * 1024
local TRACKING_GUARD_TTL_SECONDS = 300
local FLOAT32_FINGERPRINT_BUFFER = buffer.create(4)
local CAMERA_FOV_PROPERTIES = { fieldofview = true, diagonalfieldofview = true, maxaxisfieldofview = true }
local NIL_PROPERTY_BASELINE = {}

type AllowedServices = { [string]: boolean }
type DirtySeqMap = { [string]: number }
type PropertyNameSetByClass = { [string]: { [string]: string } }
type ConnectionMap = { [Instance]: { RBXScriptConnection } }
type PropertyFingerprintMap = { [Instance]: { [string]: string } }
type AttributeConnection = RBXScriptConnection | { Disconnect: (any) -> () }
-- Slot 1 holds the shared class index; the remaining slots are captured values.
type PropertyBaselines = { any }
type PropertyPrimingPlan = { [number]: { any }, slots: { [string]: number } }
type AttributeObservation = {
	active: boolean,
	generation: number,
	connections: { [Instance]: string },
	instance: Instance?,
	attribute: string?,
}
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
	cameraFovProperty: string?,
	cameraFovValue: number?,
	matchParent: boolean?,
	parent: Instance?,
	parentUnchanged: boolean?,
	profile: { [string]: number }?,
}
type ExpectedInstanceEventQueue = { ExpectedInstanceEvent }
type ExpectedInstanceEvents = { [Instance]: { [string]: ExpectedInstanceEventQueue } }
type StudioChangeLog = {
	instance: Instance?,
	firstSeq: number?,
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
	persistentTracking: boolean,
	trackingGuards: { [string]: number },
	pendingEpoch: string?,
	pendingRuntimeId: string?,
	restoredPendingEpoch: string?,
	restoredPendingRuntimeId: string?,
	seq: number,
	dirtySeqByService: DirtySeqMap,
	restoredPendingServices: { [string]: boolean },
	mutationSeqByService: DirtySeqMap,
	checkpointSeqByService: DirtySeqMap,
	fullSyncSeqByService: DirtySeqMap,
	propertyChangesByKey: { [string]: DirectPropertyChange },
	changeLogByKey: { [string]: StudioChangeLog },
	propertyFingerprintByInstance: PropertyFingerprintMap,
	parentBaselineByInstance: { [Instance]: any },
	propertyBaselineByInstance: { [Instance]: PropertyBaselines },
	ordinalCacheByParent: { [Instance]: { [Instance]: number } },
	lastParentByInstance: { [Instance]: Instance? },
	watchedServices: { [string]: boolean },
	serviceRoots: { [string]: Instance },
	serviceNameByRoot: { [Instance]: string },
	rootConnections: { [string]: { RBXScriptConnection } },
    instanceConnections: { [Instance]: AttributeConnection },
	fallbackPropertyConnections: ConnectionMap,
	connectionServiceByInstance: { [Instance]: string },
	tagFingerprintByInstance: { [Instance]: string },
	attributeObservations: { [string]: AttributeObservation },
	itemChangedAvailable: boolean,
	tagSignalsAvailable: boolean,
	tagConnections: { [string]: { RBXScriptConnection } },
	taggedInstancesByTag: { [string]: { [Instance]: boolean } },
	changeEvent: BindableEvent,
	changeSignalPending: boolean,
	waitGeneration: number,
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
	expectedProperties: { [string]: ExpectedValueQueue },
	expectedAttributes: { [string]: ExpectedValueQueue },
	expectedStructuralByInstance: ExpectedInstanceEvents,
	expectedInstanceProperties: ExpectedInstanceEvents,
	expectedInstanceAttributes: ExpectedInstanceEvents,
	expectedTags: ExpectedInstanceEvents,
	expectedFreshInstances: { [Instance]: boolean },
	expectedGeneration: number,
	luaSourceDescendantCounts: { [Instance]: number },
	archivableByInstance: { [Instance]: boolean },
	nonArchivableCountByService: { [string]: number },
	exportInstancesByService: { [string]: { Instance } },
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
	local snapshotSeq = 0
	local state: State = {
		started = false,
		persistentTracking = false,
		trackingGuards = {},
		pendingEpoch = nil,
		pendingRuntimeId = nil,
		restoredPendingEpoch = nil,
		restoredPendingRuntimeId = nil,
		seq = 0,
		dirtySeqByService = {},
		restoredPendingServices = {},
		mutationSeqByService = {},
		checkpointSeqByService = {},
		fullSyncSeqByService = {},
		propertyChangesByKey = {},
		changeLogByKey = {},
		propertyFingerprintByInstance = setmetatable({}, { __mode = "k" }) :: any,
		parentBaselineByInstance = setmetatable({}, { __mode = "k" }) :: any,
		propertyBaselineByInstance = setmetatable({}, { __mode = "k" }) :: any,
		ordinalCacheByParent = setmetatable({}, { __mode = "k" }) :: any,
		lastParentByInstance = setmetatable({}, { __mode = "k" }) :: any,
		watchedServices = {},
		serviceRoots = {},
		serviceNameByRoot = {},
		rootConnections = {},
		instanceConnections = {},
		fallbackPropertyConnections = {},
		connectionServiceByInstance = setmetatable({}, { __mode = "k" }) :: any,
		tagFingerprintByInstance = setmetatable({}, { __mode = "k" }) :: any,
		attributeObservations = {},
		itemChangedAvailable = false,
		tagSignalsAvailable = false,
		tagConnections = {},
		taggedInstancesByTag = {},
		changeEvent = Instance.new("BindableEvent"),
		changeSignalPending = false,
		waitGeneration = 0,
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
		expectedProperties = {},
		expectedAttributes = {},
		-- These own pending operations, unlike the weak observation caches above.
		-- A native-parented child can lose its Lua wrapper during bulk insertion;
		-- retain the key until consumption/cancellation and the settle cleanup.
		expectedStructuralByInstance = {},
		expectedInstanceProperties = {},
		expectedInstanceAttributes = {},
		expectedTags = {},
		expectedFreshInstances = {},
		expectedGeneration = 0,
		luaSourceDescendantCounts = setmetatable({}, { __mode = "k" }) :: any,
		archivableByInstance = setmetatable({}, { __mode = "k" }) :: any,
		nonArchivableCountByService = {},
		exportInstancesByService = {},
		changeJournal = nil,
	}

	local api = {}
	local nativeAttributeRelay = nil
	local nativeTerrainRelay = nil
	local rawTagGeneration = 0
	local rawDocumentGeneration = 0
	local tagDiscoveryConnections = {}
	function api.tagGeneration(): number
		return rawTagGeneration
	end
	local function releaseTagConnections()
		for _, connection in ipairs(tagDiscoveryConnections) do
			connection:Disconnect()
		end
		table.clear(tagDiscoveryConnections)
		for _, connections in pairs(state.tagConnections) do
			for _, connection in ipairs(connections) do
				connection:Disconnect()
			end
		end
		table.clear(state.tagConnections)
		table.clear(state.taggedInstancesByTag)
		state.tagSignalsAvailable = false
	end
	local function releaseProofConnections(relay)
		for _, connection in ipairs(relay.proofConnections or {}) do
			connection:Disconnect()
		end
		relay.proofConnections = nil
	end
	function api.nativeAttributeProof(): any
		local relay = nativeAttributeRelay
		if relay == nil or not relay.nativeArmed then return nil end
		return { id = relay.notify.Name, generation = relay.attributeGeneration }
	end
	local verifiedPushProof = nil
	local localPushObservation = nil
	local localPushProofRequested = false
	local nextPushProof = 0
	local function releaseLocalPushObservation()
		if localPushObservation == nil then return end
		state.trackingGuards[localPushObservation.guardId] = nil
		releaseProofConnections(localPushObservation)
		localPushObservation = nil
		verifiedPushProof = nil
	end
	local function proofDocuments(services: { string }): any
		-- A document may close while Studio is retiring its script editor.
		local ok, entries = pcall(BridgeScriptDocuments.capture, services)
		if not ok then return nil end
		local documents = {}
		for _, entry in ipairs(entries) do
			documents[entry.instance] = entry.source
		end
		return documents
	end
	local function matchesPushState(proof): (boolean, string?)
		if proof.localObservation ~= nil then
			if proof.localObservation ~= localPushObservation or not state.started or state.onlyCodeMode
				or not state.syncbackProperties or not state.itemChangedAvailable then
				return false, "local observation ended"
			end
			if proof.camera ~= Workspace.CurrentCamera or proof.cameraGeneration ~= localPushObservation.cameraGeneration then
				return false, "viewport attributes changed"
			end
			for service, entry in pairs(proof.attributes) do
				local observation = api.attributeObservation(service)
				if observation ~= entry.observation or observation.generation ~= entry.generation then
					return false, "attributes changed"
				end
			end
		else
			local attributes = api.nativeAttributeProof()
			if attributes == nil then return false, "native observation ended" end
			if attributes.id ~= proof.attributes.id or attributes.generation ~= proof.attributes.generation then
				return false, "attributes changed"
			end
		end
		if proof.tags ~= rawTagGeneration then return false, "tags changed" end
		if proof.documentGeneration ~= rawDocumentGeneration then return false, "script editor documents changed" end
		for service, generation in pairs(proof.generations) do
			local current, lastChange = config.exportProofGeneration(service)
			if current ~= generation then
				return false, `{service} {lastChange or "properties or hierarchy"} changed`
			end
		end
		local documents = proofDocuments(proof.services)
		if documents == nil then return false, "script editor documents unavailable" end
		for instance, source in pairs(documents) do
			if proof.documents[instance] ~= source then return false, "script editor buffer changed" end
		end
		for instance, source in pairs(proof.documents) do
			if documents[instance] ~= source then return false, "script editor buffer changed" end
		end
		return true, nil
	end
	local function proofTerrain(required: boolean): string?
		if not required then return "" end
		if config.captureTerrainProof == nil then return nil end
		-- Terrain voxel writes have no ordinary property event. Native
		-- serialization can fail or yield while the DataModel is closing.
		local ok, digest = pcall(config.captureTerrainProof)
		return if ok then digest else nil
	end
	function api.capturePushProof(): (number?, string?)
		local attributes = api.nativeAttributeProof()
		if config.exportProofGeneration == nil or config.observeProofDocuments == nil
			or CollectionService.TagAdded == nil or CollectionService.TagRemoved == nil then return nil, "proof observers unavailable" end
		local relay = nativeAttributeRelay
		local localObservation = nil
		if attributes == nil then
			-- macOS uses the existing local attribute subscriptions. Keep their
			-- exact observation epochs, not the filtered Live Sync dirty counts.
			if not localPushProofRequested or not state.started or state.onlyCodeMode or not state.syncbackProperties or not state.itemChangedAvailable then return nil, "full local observation unavailable" end
			if localPushObservation == nil then
				localPushObservation = { services = table.clone(state.watchedServices), cameraGeneration = 0,
					guardId = "push-proof-" .. tostring(nextPushProof + 1) }
			end
			relay = localPushObservation
			localObservation = relay
			relay.services = table.clone(state.watchedServices)
			if relay.camera ~= Workspace.CurrentCamera then
				releaseProofConnections(relay)
				relay.camera = Workspace.CurrentCamera
			end
			attributes = {}
			for service in pairs(relay.services) do
				local observation = api.attributeObservation(service)
				if observation == nil then return nil, `{service} attribute observation unavailable` end
				attributes[service] = { observation = observation, generation = observation.generation }
			end
		end
		if relay.proofConnections == nil then
			relay.proofConnections = config.observeProofDocuments(function()
				rawDocumentGeneration += 1
			end)
			-- The active camera is deliberately excluded from ordinary sync
			-- listeners, but its authored attributes are part of the export.
			if localObservation ~= nil and Workspace.CurrentCamera ~= nil then
				table.insert(relay.proofConnections, Workspace.CurrentCamera.AttributeChanged:Connect(function()
					relay.cameraGeneration += 1
					local observation = state.attributeObservations.Workspace
					if observation and observation.active then
						observation.generation += 1
						observation.instance = relay.camera
					end
					config.invalidateRuntimeExportCache("Workspace")
				end))
			end
		end
		local services, generations = {}, {}
		for service in pairs(relay.services) do
			services[#services + 1] = service
			generations[service] = config.exportProofGeneration(service)
		end
		local documents = proofDocuments(services)
		if documents == nil then return nil, "script editor documents unavailable" end
		local proof = { attributes = attributes, services = services, generations = generations, documents = documents,
			tags = rawTagGeneration, documentGeneration = rawDocumentGeneration, terrainRequired = relay.services.Workspace == true,
			localObservation = localObservation, camera = Workspace.CurrentCamera, cameraGeneration = relay.cameraGeneration }
		proof.terrain = proofTerrain(proof.terrainRequired)
		-- The serializer yields. All other state must remain unchanged across
		-- that interval, including open-buffer changes that later cancel out.
		if proof.terrain == nil then return nil, "Terrain proof unavailable" end
		local matches, reason = matchesPushState(proof)
		if not matches then return nil, reason end
		nextPushProof += 1
		proof.id = nextPushProof
		verifiedPushProof = proof
		return proof.id, nil
	end
	function api.verifyPushProof(id: any): (boolean, string?)
		local proof = verifiedPushProof
		if proof == nil or proof.id ~= id then return false, "proof retired" end
		local matches, reason = matchesPushState(proof)
		if not matches then return false, reason end
		local terrain = proofTerrain(proof.terrainRequired)
		if terrain == nil then return false, "Terrain unavailable" end
		if terrain ~= proof.terrain then return false, "Terrain changed" end
		return matchesPushState(proof)
	end
	local nativeAttributeConnection = { Disconnect = function(_self: any) end }
	-- Native receipts already retain initial parent identities. Materialize a
	-- separate baseline only when that object actually receives a Parent event.
	local nativeParentReceipts = {}
	local function nativeInitialParent(instance: Instance): Instance?
		local serviceName = state.connectionServiceByInstance[instance]
		for index = #nativeParentReceipts, 1, -1 do
			local instances = nativeParentReceipts[index][serviceName]
			if instances and instances[instance] then return instances[instance] end
		end
		return nil
	end
	local promoteAttributeConnection: (Instance) -> ()
	local ensureTracking: ({ string }) -> ()
	local serviceSignals: { [string]: { connections: { RBXScriptConnection }, added: ((Instance, boolean?) -> ())?, removing: ((Instance) -> ())? } } = {}
	local propertySignal: RBXScriptConnection? = nil
	local exportStructureObserver: ((string, Instance) -> boolean?)? = nil
	local exportPropertyObserver: ((Instance, any) -> ())? = nil
	local propertyReadNamesByClass: { [string]: { [string]: string } } = {}
	local propertyCacheKeysByClass: { [string]: { [string]: string } } = {}
	local propertyPrimingPlansByClass: { [string]: PropertyPrimingPlan } = {}
	local valuePropertySignalNamesByClass: { [string]: { string } } = {}
	local propertyEventRelevanceByClass: { [string]: { [string]: boolean } } = {}
	local leasedWaits: { [BindableEvent]: { leaseId: string, cancel: () -> () } } = {}
	local luaSourceClasses = config.LUA_SOURCE_CLASS
	local rebuildExportInstances
	local prepareParentObservers
	local releaseJournalObservers
	local changeIdentityByInstance = setmetatable({}, { __mode = "k" })
	local nextChangeIdentity = 0
	local pendingView = nil
	local reportedSeq = 0
	local persistPendingServices

	local function changeIdentity(instance: Instance): string
		local identity = changeIdentityByInstance[instance]
		if identity == nil then
			nextChangeIdentity += 1
			identity = tostring(nextChangeIdentity)
			changeIdentityByInstance[instance] = identity
		end
		return identity
	end

	local function pendingChanges()
		if pendingView ~= nil then
			return pendingView
		end
		local changes = {}
		local counts = {}
		local discard = {}
		for key, entry in pairs(state.changeLogByKey) do
			local instance = entry.instance
			local prefix = entry.service .. "\0"
			local identity = if instance ~= nil then changeIdentity(instance) .. "\0" else nil
			local added = if identity ~= nil then state.changeLogByKey[prefix .. "added\0" .. identity] else nil
			local removed = if identity ~= nil then state.changeLogByKey[prefix .. "removed\0" .. identity] else nil
			-- Retain the records internally until acknowledgment: an earlier snapshot
			-- may still commit the addition, in which case its deletion becomes pending.
			local cancelled = added ~= nil and removed ~= nil
				and (added.firstSeq or added.seq) > 0
				and (added.firstSeq or added.seq) < (removed.firstSeq or removed.seq)
				and added.seq < removed.seq
			if not cancelled then
				changes[#changes + 1] = entry
				counts[entry.service] = (counts[entry.service] or 0) + 1
			elseif (added.firstSeq or added.seq) > reportedSeq then
				-- No command could have captured this addition. Forget the cancelled
				-- records now instead of accumulating them during rapid insert/delete.
				discard[#discard + 1] = key
			end
		end
		local cleared = false
		for _, key in ipairs(discard) do
			local serviceName = state.changeLogByKey[key].service
			state.changeLogByKey[key] = nil
			state.changeLogCountByService[serviceName] -= 1
			if state.changeLogCountByService[serviceName] == 0 then
				state.dirtySeqByService[serviceName] = nil
				state.fullSyncSeqByService[serviceName] = nil
				cleared = true
			end
		end
		if cleared then
			persistPendingServices()
		end
		for serviceName in pairs(state.dirtySeqByService) do
			if (state.changeLogCountByService[serviceName] or 0) == 0 then
				counts[serviceName] = 1
			end
		end
		pendingView = { changes = changes, counts = counts }
		return pendingView
	end

	persistPendingServices = function()
		local services = {}
		for serviceName in pairs(state.dirtySeqByService) do
			services[#services + 1] = serviceName
		end
		table.sort(services)
		state.pendingEpoch = config.savePendingStudioChanges(services, state.pendingEpoch)
		state.pendingRuntimeId = if state.pendingEpoch ~= nil then config.bridgeRuntimeId else nil
	end

	local function clearRestoredPendingService(serviceName: string)
		state.restoredPendingServices[serviceName] = nil
		if next(state.restoredPendingServices) == nil then
			state.restoredPendingEpoch = nil
			state.restoredPendingRuntimeId = nil
		end
	end

	local pending = config.loadPendingStudioChanges()
	if type(pending) == "table" and type(pending.services) == "table" then
		state.pendingEpoch = pending.epoch
		state.pendingRuntimeId = pending.runtimeId
		state.restoredPendingEpoch = pending.epoch
		state.restoredPendingRuntimeId = pending.runtimeId
		for _, serviceName in pending.services do
			if type(serviceName) == "string" and allowedServices[serviceName] then
				state.seq += 1
				state.dirtySeqByService[serviceName] = state.seq
				state.restoredPendingServices[serviceName] = true
				state.mutationSeqByService[serviceName] = state.seq
				state.checkpointSeqByService[serviceName] = state.seq
				state.fullSyncSeqByService[serviceName] = state.seq
				state.changeLogByKey[serviceName .. "\0restored"] = {
					service = serviceName, action = "fullSync", path = serviceName,
					reason = "pending changes restored after reconnect", fullSync = true, seq = state.seq,
				}
				state.changeLogCountByService[serviceName] = 1
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
		return serviceName .. "\0" .. structuredPathKey(pathSegments, pathOrdinals) .. "\0" .. tostring(name or "")
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
	local primeExpectedProperty: (Instance, string) -> ()

	local function consumeExpectedInstanceEvent(
		target: ExpectedInstanceEvents,
		instance: Instance,
		action: string,
		fingerprint: string?,
		parent: Instance?
	): (boolean, ExpectedInstanceEvent?)
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
				(if event.cameraFovProperty
					then BridgeValueEquality.valuesEqual((instance :: any)[event.cameraFovProperty], event.cameraFovValue)
					else event.fingerprint == nil or event.fingerprint == fingerprint)
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
				return true, event
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

	function api.expectParentChange(instance: Instance, nextParent: Instance?, profile: { [string]: number }?, serializedInsertion: boolean?)
		local tokens = {}
		local wasInDataModel = instance:IsDescendantOf(game)
		local willBeInDataModel = nextParent ~= nil and (nextParent == game or nextParent:IsDescendantOf(game))
		local tagAction = if not wasInDataModel and willBeInDataModel
			then "added:"
			else if wasInDataModel and not willBeInDataModel then "removed:" else nil
		local instances = {}
		-- Detached payload wrappers never emit service structural/tag signals.
		-- Keep the root Parent expectation below for still-connected deferred observers.
		if wasInDataModel or willBeInDataModel then
			instances[1] = instance
			for _, descendant in ipairs(instance:GetDescendants()) do
				instances[#instances + 1] = descendant
			end
		end
		for _, target in ipairs(instances) do
			if not wasInDataModel and willBeInDataModel then
				state.expectedFreshInstances[target] = true
			end
			if wasInDataModel then
				tokens[#tokens + 1] =
					expectInstanceEvent(state.expectedStructuralByInstance, target, "removed", nil, false, nil)
			end
			if willBeInDataModel then
				local expectedParent = if target == instance then nextParent else target.Parent
				local token = expectInstanceEvent(state.expectedStructuralByInstance, target, "added", nil, true, expectedParent)
				-- Attaching a root does not change its descendants' sibling order.
				-- Preserve that cache across the resulting DescendantAdded fan-out.
				token.parentUnchanged = target ~= instance
				token.profile = profile
				tokens[#tokens + 1] = token
			end
			if tagAction ~= nil then
				for _, tag in ipairs(CollectionService:GetTags(target)) do
					local action = if tag == "Tags" then "property" else tagAction .. tag
					tokens[#tokens + 1] = expectInstanceEvent(state.expectedTags, target, action, nil, false, nil)
				end
			end
		end
		-- DescendantAdded connects a newly parented instance before Roblox may emit its
		-- deferred Parent change. Register this expectation even while the instance is
		-- detached so Renium cannot mistake its own create for a Studio edit.
		tokens[#tokens + 1] = expectInstanceEvent(state.expectedInstanceProperties, instance, "parent",
			expectedPropertyFingerprint(instance, "Parent", nextParent), false, nil)
		local token = { tokens = tokens }
		if not wasInDataModel and willBeInDataModel and not state.onlyCodeMode then
			prepareParentObservers(instances, nextParent, token, profile, serializedInsertion)
		elseif wasInDataModel and not willBeInDataModel and state.changeJournal ~= nil then
			prepareParentObservers(instances, nil, token, profile)
		end
		return token
	end

	function api.expectPropertyEvent(instance: Instance, propertyName: string, value: any)
		propertyName = RbxDomModule.getContentPropertyAliases(instance.ClassName)[string.lower(propertyName)] or propertyName
		if string.lower(propertyName) == "enabled" and instance:IsA("BaseScript") and type(value) == "boolean" then
			return api.expectPropertyEvent(instance, "Disabled", not value)
		end
		if string.lower(propertyName) == "ignoreguiinset" and instance:IsA("ScreenGui") and type(value) == "boolean" then
			local current = (instance :: ScreenGui).ScreenInsets
			local insets = if not value then Enum.ScreenInsets.CoreUISafeInsets
				elseif current == Enum.ScreenInsets.CoreUISafeInsets then Enum.ScreenInsets.DeviceSafeInsets else current
			return api.expectPropertyEvent(instance, "ScreenInsets", insets)
		end
		if string.lower(propertyName) == "brickcolor" and instance:IsA("BasePart") and typeof(value) == "BrickColor" then
			return api.expectPropertyEvent(instance, "Color", value.Color)
		end
		if instance.ClassName == "Camera" and CAMERA_FOV_PROPERTIES[string.lower(propertyName)] and type(value) == "number" then
			-- FOV setters round through the engine's projection. Match the requested
			-- field using the same equality as write verification, not exact hashes.
			primeExpectedProperty(instance, "FieldOfView")
			local token = expectInstanceEvent(state.expectedInstanceProperties, instance, "fieldofview", nil, false, nil)
			token.cameraFovProperty = propertyName
			token.cameraFovValue = value
			return token
		end
		if instance.ClassName == "UICorner" and propertyName == "CornerRadius" then
			local tokens = {}
			for _, name in { "TopLeftRadius", "TopRightRadius", "BottomLeftRadius", "BottomRightRadius" } do
				tokens[#tokens + 1] = api.expectPropertyEvent(instance, name, value)
			end
			return { tokens = tokens }
		end
		primeExpectedProperty(instance, propertyName)
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
		-- Own post-apply writes need the ordinary per-attribute expectations.
		promoteAttributeConnection(instance)
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
		if token.complete then
			token.complete(true)
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

	function api.checkpointGeneration(serviceName: string): number
		return state.checkpointSeqByService[serviceName] or 0
	end

	function api.isTracking(serviceName: string): boolean
		return state.started and state.watchedServices[serviceName] == true
	end

	function api.trackedServiceRoot(instance: Instance): Instance?
		local serviceName = state.connectionServiceByInstance[instance]
		local root = if serviceName then state.serviceRoots[serviceName] else nil
		-- Parent changes can precede deferred connection updates. Never return
		-- the former service after a move or while an object is detached.
		return if root ~= nil and instance:IsDescendantOf(root) then root else nil
	end

	function api.attributeObservation(serviceName: string): AttributeObservation?
		if not api.isTracking(serviceName) or state.onlyCodeMode then
			return nil
		end
		local observation = state.attributeObservations[serviceName]
		if observation == nil or not observation.active then
			observation = { active = true, generation = 0, connections = state.connectionServiceByInstance }
			state.attributeObservations[serviceName] = observation
		end
		return observation
	end

	function api.hasNonArchivable(serviceName: string): boolean?
		-- Native commit can invalidate before deferred structural signals run.
		-- Refresh both views together instead of consulting an older count.
		if state.onlyCodeMode or api.exportInstances(serviceName) == nil then
			return nil
		end
		return (state.nonArchivableCountByService[serviceName] or 0) > 0
	end

	function api.exportInstances(serviceName: string): { Instance }?
		if not api.isTracking(serviceName) then
			return nil
		end
		local instances = state.exportInstancesByService[serviceName]
		if instances == nil then
			instances = rebuildExportInstances(state.serviceRoots[serviceName], serviceName)
		end
		return instances
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
			detachedInstances = {},
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
		if journal.nativeAdditions ~= nil then
			error("Native import creation receipt is still pending")
		end
		local records = journal.records
		journal.records = {}
		journal.recordsByInstance = {}
		return records
	end

	function api.finishChangeJournal(transactionId: string): { any }
		-- An interrupted native import has no accepted creation receipt. Flush
		-- its pending additions as outside edits, never silently discard them.
		api.finishNativeImportObservations(transactionId, nil)
		local records = api.drainChangeJournal(transactionId)
		local journal = state.changeJournal
		state.changeJournal = nil
		if journal ~= nil then
			releaseJournalObservers(journal)
		end
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
		pendingView = nil
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

	local function signalTrackedChange()
		if state.changeSignalPending then
			return
		end
		state.changeSignalPending = true
		task.defer(function()
			state.changeSignalPending = false
			if state.started then
				signalChange()
			end
		end)
	end

	local function hasPendingChanges(services: { string }): boolean
		local counts = pendingChanges().counts
		for _, serviceName in ipairs(services) do
			if state.dirtySeqByService[serviceName] ~= nil and counts[serviceName] ~= nil then
				return true
			end
		end
		return type(config.hasPendingBridgeSettingChanges) == "function"
			and config.hasPendingBridgeSettingChanges()
	end

	local function waitForDirtyServices(services: { string }, waitSeconds: number?, leaseId: string?): (boolean, boolean)
		local duration = tonumber(waitSeconds) or 0
		if duration <= 0 then
			return hasPendingChanges(services), false
		end
		if hasPendingChanges(services) then
			return true, false
		end
		duration = math.min(duration, 25)
		local waitGeneration = state.waitGeneration

		local wakeEvent = Instance.new("BindableEvent")
		local done = false
		local timedOut = false
		local cancelled = false
		if leaseId then
			leasedWaits[wakeEvent] = { leaseId = leaseId, cancel = function()
				cancelled = true
				wakeEvent:Fire("cancel")
			end }
		end
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

		while
			state.started
			and state.waitGeneration == waitGeneration
			and not cancelled
			and not timedOut
			and os.clock() < deadline
			and not hasPendingChanges(services)
		do
			wakeEvent.Event:Wait()
		end

		done = true
		leasedWaits[wakeEvent] = nil
		connection:Disconnect()
		wakeEvent:Destroy()
		return hasPendingChanges(services), cancelled or state.waitGeneration ~= waitGeneration
	end

	local function pathToString(pathSegments: { string }?): string?
		if pathSegments == nil or #pathSegments == 0 then
			return nil
		end
		return table.concat(pathSegments, ".")
	end

	local function recordChange(serviceName: string, requiresFullSync: boolean, details: StudioChangeDetails?)
		pendingView = nil
		local entry: StudioChangeLog = {
			service = serviceName,
			action = if requiresFullSync then "fullSync" else "property",
			seq = state.seq,
			fullSync = requiresFullSync,
		}
		if details ~= nil then
			entry.instance = details.instance
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
		local pathKey = if entry.instance ~= nil then changeIdentity(entry.instance)
			else if structuredKey == "" then entry.path or serviceName else structuredKey
		local key = serviceName
			.. "\0"
			.. entry.action
			.. "\0"
			.. tostring(pathKey)
			.. "\0"
			.. tostring(entry.property or entry.attribute or "")
		local previous = state.changeLogByKey[key]
		entry.firstSeq = if previous ~= nil then previous.firstSeq or previous.seq else entry.seq
		if state.changeLogByKey[key] == nil then
			local retainedCount = state.changeLogCountByService[serviceName] or 0
			if retainedCount >= MAX_CHANGE_LOGS_PER_SERVICE then
				pendingChanges()
				pendingView = nil
				retainedCount = state.changeLogCountByService[serviceName] or 0
				if state.dirtySeqByService[serviceName] == nil then
					state.dirtySeqByService[serviceName] = state.seq
					if requiresFullSync then
						state.fullSyncSeqByService[serviceName] = state.seq
					end
					persistPendingServices()
				end
			end
			if retainedCount >= MAX_CHANGE_LOGS_PER_SERVICE then
				clearChangeLogsForService(serviceName)
				clearPropertyChangesForService(serviceName)
				state.fullSyncSeqByService[serviceName] = state.seq
				entry = {
					service = serviceName,
					action = "fullSync",
					reason = "change log retention limit reached",
					path = serviceName,
					fullSync = true,
					seq = state.seq,
				}
				key = serviceName .. "\0fullSync\0retention-limit\0"
			end
			state.changeLogCountByService[serviceName] = (state.changeLogCountByService[serviceName] or 0) + 1
		end
		state.changeLogByKey[key] = entry
	end

	local function recordJournalChange(serviceName: string, details: StudioChangeDetails?): boolean
		local journal = state.changeJournal
		local instance = if details ~= nil then details.instance else nil
		if journal == nil or not journal.services[serviceName] or instance == nil or typeof(instance) ~= "Instance" then
			return false
		end
		local record = journal.recordsByInstance[instance]
		if record == nil then
			record = {
				instance = instance,
				service = serviceName,
				services = { [serviceName] = true },
				properties = {},
				attributes = {},
			}
			journal.recordsByInstance[instance] = record
			journal.records[#journal.records + 1] = record
		else
			record.services = record.services or { [record.service] = true }
			record.services[serviceName] = true
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
		elseif action == "attributes" then
			-- Native Attributes reports the object, not the individual key. Capture
			-- the event-time map, including deletions, without priming every object.
			record.attributesSnapshot = details.journalValue
			table.clear(record.attributes)
		elseif action == "tag" then
			record.tagsChanged = true
		elseif action == "added" or action == "removed" then
			record.structural = true
		end
		return true
	end

	local function markDirty(serviceName: string?, details: StudioChangeDetails?)
		if not state.started or serviceName == nil or not allowedServices[serviceName] then
			return
		end
		if isExpectedChange(serviceName, details) then
			return
		end
		recordJournalChange(serviceName, details)
		local wasDirty = state.dirtySeqByService[serviceName] ~= nil
		state.seq += 1
		state.dirtySeqByService[serviceName] = state.seq
		clearRestoredPendingService(serviceName)
		state.mutationSeqByService[serviceName] = state.seq
		state.checkpointSeqByService[serviceName] = state.seq
		state.fullSyncSeqByService[serviceName] = state.seq
		clearPropertyChangesForService(serviceName)
		recordChange(serviceName, true, details)
		if not wasDirty then
			persistPendingServices()
		end
		-- One Studio operation can fire structural, property, and attribute callbacks
		-- back-to-back. Wake the daemon after that callback turn so it exports the
		-- complete operation atomically.
		signalTrackedChange()
	end

	function api.beginNativeImportObservations(transactionId: string, profile: { [string]: number }?, untaggedClasses: { [string]: boolean }?)
		local journal = state.changeJournal
		if journal == nil or journal.id ~= transactionId or journal.nativeAdditions ~= nil then
			error("Native import observation requires its exclusive editor transaction")
		end
		local additions = {}
		for serviceName in pairs(journal.services) do
			additions[serviceName] = {}
		end
		journal.nativeAdditions = additions
		journal.nativeProfile = profile
		journal.nativeUntaggedClasses = untaggedClasses
		nativeParentReceipts[#nativeParentReceipts + 1] = additions
		if config.syncProfile then
			config.syncProfile.nativeImport = profile
		end
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
		instance: Instance,
		action: string,
		name: string
	): string
		return serviceName .. "\0" .. action .. "\0" .. changeIdentity(instance) .. "\0" .. name
	end

	local function removeQueuedDirectChange(
		instance: Instance,
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
		local logKey = directChangeLogKey(serviceName, instance, scope, name)
		if state.changeLogByKey[logKey] ~= nil then
			pendingView = nil
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
			clearRestoredPendingService(serviceName)
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
			return true,
				{
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

	local function prepareNativeTerrainRelay(): { string }
		if nativeTerrainRelay then return nativeTerrainRelay.path end
		local notify = Instance.new("BoolValue")
		notify.Name = "ReniumTerrainChanges_" .. game:GetService("HttpService"):GenerateGUID(false)
		notify.Archivable = false
		local relay = { notify = notify, path = { "CoreGui", notify.Name } }
		nativeTerrainRelay = relay
		relay.connection = notify:GetPropertyChangedSignal("Value"):Connect(function()
			if nativeTerrainRelay ~= relay or not notify.Value then return end
			config.invalidateTerrainExport()
			if state.watchedServices.Workspace and state.syncbackProperties and not state.onlyCodeMode then
				markDirty("Workspace", changeDetailsForInstance(Workspace.Terrain, "property", "SmoothGrid", nil, "terrain changed"))
			end
			task.defer(function()
				if nativeTerrainRelay == relay then
					notify.Value = false
				end
			end)
		end)
		notify.Parent = game:GetService("CoreGui")
		return relay.path
	end

	function api.finishNativeImportObservations(transactionId: string, createdById: { [string]: string }?): { [string]: { [Instance]: Instance } }
		local journal = state.changeJournal
		if journal == nil then
			return {}
		end
		if journal.id ~= transactionId then
			error("Native import observation belongs to another transaction")
		end
		local additions = journal.nativeAdditions
		if additions == nil then
			return {}
		end
		journal.nativeAdditions = nil
		journal.nativeProfile = nil
		journal.nativeUntaggedClasses = nil
		if config.syncProfile then
			config.syncProfile.nativeImport = nil
		end
		for serviceName, instances in pairs(additions) do
			for instance, parent in pairs(instances) do
				local class = if createdById then createdById[BridgeIdentity.getDebugId(instance)] else nil
				if class ~= instance.ClassName then
					local details = changeDetailsForInstance(instance, "added", nil, nil, "descendant added")
					details.reason = `unowned native insertion: {instance.Name} ({instance.ClassName}), parent {tostring(parent)}`
					markDirty(serviceName, details)
					instances[instance] = nil
				end
			end
		end
		return additions
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
			-- Structural events own removals and cross-service moves. Late property
			-- or attribute callbacks must not enqueue a second edit at a missing path.
			-- Transaction-owned detached objects still need their outside edits for
			-- rollback; retain identity/value without publishing a nonexistent path.
			recordJournalChange(serviceName, {
				instance = instance, action = scope,
				property = if scope == "property" then name else nil,
				attribute = if scope == "attribute" then name else nil,
				journalValueCaptured = journalValueCaptured, journalValue = journalValue,
			})
			return true
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
			removeQueuedDirectChange(instance, serviceName, pathSegments, pathOrdinals, scope, name)
			return true
		end
		local journalDetails = {
			instance = instance,
			action = scope,
			property = if scope == "property" then name else nil,
			attribute = if scope == "attribute" then name else nil,
			pathSegments = pathSegments,
			pathOrdinals = pathOrdinals,
			journalValueCaptured = journalValueCaptured,
			journalValue = journalValue,
			value = nil,
		}
		recordJournalChange(serviceName, journalDetails)
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
			markDirty(serviceName, details)
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
		if nextCount > MAX_DIRECT_PROPERTY_CHANGES or nextBytes > MAX_DIRECT_PROPERTY_BYTES then
			markDirty(serviceName, {
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
		clearRestoredPendingService(serviceName)
		state.mutationSeqByService[serviceName] = state.seq
		state.checkpointSeqByService[serviceName] = state.seq
		recordChange(serviceName, false, {
			instance = instance,
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
		signalTrackedChange()
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

	local includeExportInstance: (string, Instance) -> boolean

	local function shouldIgnoreInstance(instance: Instance, serviceName: string?, exportIncluded: boolean?): boolean
		if config.shouldIgnoreInstance(instance) then
			return true
		end
		if serviceName ~= nil and (exportIncluded == false or exportIncluded == nil and not includeExportInstance(serviceName, instance)) then
			return true
		end
		return instance == Workspace.CurrentCamera
	end

	includeExportInstance = function(serviceName: string, instance: Instance): boolean
		local callback = config.includeExportInstance
		if type(callback) == "function" then
			return callback(serviceName, instance)
		end
		return not config.shouldIgnoreInstance(instance)
	end

	local function updateTrackedArchivable(instance: Instance, serviceName: string, exportIncluded: boolean?)
		if exportIncluded == false or exportIncluded == nil and not includeExportInstance(serviceName, instance) then
			return
		end
		local current = instance.Archivable
		local previous = state.archivableByInstance[instance]
		-- True is the default. Store only non-Archivable exceptions; full native
		-- insertion otherwise grows this weak hash table once per new object.
		if (previous ~= false) == current then
			return
		end
		local count = state.nonArchivableCountByService[serviceName] or 0
		if previous == false then
			count -= 1
		end
		if current == false then
			count += 1
		end
		state.archivableByInstance[instance] = if current then nil else false
		state.nonArchivableCountByService[serviceName] = count
	end

	local function removeTrackedArchivable(instance: Instance, serviceName: string)
		if state.archivableByInstance[instance] == false then
			state.nonArchivableCountByService[serviceName] =
				math.max(0, (state.nonArchivableCountByService[serviceName] or 0) - 1)
		end
		state.archivableByInstance[instance] = nil
	end

	rebuildExportInstances = function(
		service: Instance,
		serviceName: string,
		descendants: { Instance }?
	): { Instance }
		local instances = { service }
		local count = 0
		for _, instance in ipairs(descendants or service:GetDescendants()) do
			if includeExportInstance(serviceName, instance) then
				instances[#instances + 1] = instance
				local archivable = instance.Archivable
				state.archivableByInstance[instance] = if archivable then nil else false
				if not archivable then
					count += 1
				end
			end
		end
		state.nonArchivableCountByService[serviceName] = count
		state.exportInstancesByService[serviceName] = instances
		return instances
	end

	function api.invalidateExportCache(serviceName: string)
		if not allowedServices[serviceName] then
			return
		end
		state.exportInstancesByService[serviceName] = nil
	end

	local function isLuaSourceInstance(instance: Instance): boolean
		return luaSourceClasses[instance.ClassName]
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

	local function rebuildLuaSourceCounts(root: Instance, descendants: { Instance })
		state.luaSourceDescendantCounts[root] = if isLuaSourceInstance(root) then 1 else 0
		for _, instance in ipairs(descendants) do
			state.luaSourceDescendantCounts[instance] = if isLuaSourceInstance(instance) then 1 else 0
		end
		for index = #descendants, 1, -1 do
			local instance = descendants[index]
			local count = state.luaSourceDescendantCounts[instance]
			if count > 0 then
				local parent = instance.Parent
				if parent ~= nil then
					state.luaSourceDescendantCounts[parent] = (state.luaSourceDescendantCounts[parent] or 0) + count
				end
			end
		end
	end

	local function exportPropertyNameForEvent(instance: Instance, loweredPropertyName: string): string
		if loweredPropertyName == "enabled" and instance:IsA("BaseScript") then
			return "disabled"
		end
		if loweredPropertyName == "ignoreguiinset" and instance:IsA("ScreenGui") then
			return "screeninsets"
		end
		local contentName = RbxDomModule.getContentPropertyAliases(instance.ClassName)[loweredPropertyName]
		if contentName then
			return string.lower(contentName)
		end
		if instance.ClassName == "Camera" and CAMERA_FOV_PROPERTIES[loweredPropertyName] then
			return "fieldofview"
		end
		if loweredPropertyName == "brickcolor" or loweredPropertyName == "color3uint8" then
			if instance:IsA("BasePart") then
				return "color"
			end
		elseif loweredPropertyName == "position" or loweredPropertyName == "orientation" or loweredPropertyName == "rotation" then
			if instance:IsA("BasePart") then
				return "cframe"
			end
		elseif loweredPropertyName == "worldpivotdata" then
			if instance:IsA("Model") or instance:IsA("WorldModel") then
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

		local events = propertyEventRelevanceByClass[instance.ClassName]
		if events == nil then
			events = {}
			propertyEventRelevanceByClass[instance.ClassName] = events
		end
		local cached = events[lowered]
		if cached ~= nil then return cached end
		local exportPropertyName = exportPropertyNameForEvent(instance, lowered)
		local relevant = classProperties[exportPropertyName] ~= nil
		events[lowered] = relevant
		return relevant
	end

	local function serviceNameForTrackedInstance(instance: Instance): string?
		if shouldIgnoreInstance(instance) then
			return nil
		end
		local current: Instance? = instance
		while current ~= nil and current ~= game do
			local serviceName = state.serviceNameByRoot[current]
			if serviceName ~= nil then
				return if shouldIgnoreInstance(instance, serviceName) then nil else serviceName
			end
			current = current.Parent
		end
		return nil
	end

	local function tagChangeRelevant(instance: Instance): boolean
		return not state.onlyCodeMode or hasLuaSourceDescendant(instance)
	end

	local function tagFingerprint(instance: Instance): string
		local tags = CollectionService:GetTags(instance)
		if #tags == 0 then return "" end
		table.sort(tags)
		return table.concat(tags, "\0")
	end

	local function markTagChange(instance: Instance, tag: string, added: boolean)
		rawTagGeneration += 1
		if nativeAttributeRelay and nativeAttributeRelay.cached then return end
		local fingerprint = tagFingerprint(instance)
		local previous = state.tagFingerprintByInstance[instance]
		if previous == nil and (state.connectionServiceByInstance[instance] ~= nil or state.serviceNameByRoot[instance] ~= nil) then
			previous = ""
		end
		state.tagFingerprintByInstance[instance] = if fingerprint == "" then nil else fingerprint
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
		-- CollectionService reports existing memberships when a tree enters the
		-- DataModel and when a newly discovered tag gets its first listener.
		-- Those are not edits to the tags captured when tracking began.
		if previous == fingerprint then
			return
		end
		local serviceName = serviceNameForTrackedInstance(instance)
		if serviceName == nil or not tagChangeRelevant(instance) then
			return
		end
		markDirty(
			serviceName,
			changeDetailsForInstance(instance, "tag", "Tags", nil, if added then "tag added" else "tag removed")
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

	local function observeTagDiscovery()
		if #tagDiscoveryConnections > 0 then return end
		tagDiscoveryConnections = {
			CollectionService.TagAdded:Connect(function(tag: string)
				rawTagGeneration += 1
				connectTag(tag, true)
			end),
			CollectionService.TagRemoved:Connect(function(tag: string)
				rawTagGeneration += 1
				-- The last membership and the global tag removal may be delivered
				-- in either order. Record the live fingerprint before retiring the
				-- membership listeners; duplicate notifications remain no-ops.
				for instance in pairs(state.taggedInstancesByTag[tag] or {}) do
					markTagChange(instance, tag, false)
				end
				for _, connection in ipairs(state.tagConnections[tag] or {}) do
					connection:Disconnect()
				end
				state.tagConnections[tag] = nil
				state.taggedInstancesByTag[tag] = nil
			end),
		}
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
				parts[#parts + 1] = stableValueString(key, currentDepth + 1)
					.. "="
					.. stableValueString(child, currentDepth + 1)
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
		local className = instance.ClassName
		local keys = propertyCacheKeysByClass[className]
		if keys == nil then
			keys = {}
			propertyCacheKeysByClass[className] = keys
		end
		local cached = keys[propertyName]
		if cached ~= nil then
			return cached
		end
		local lowered = string.lower(propertyName)
		local key = if ATTRIBUTE_EVENT_PROPERTIES[lowered] then "attributes"
			else "property:" .. exportPropertyNameForEvent(instance, lowered)
		keys[propertyName] = key
		return key
	end

	local function uncachedPropertyReadName(instance: Instance, propertyName: string): string
		local lowered = string.lower(propertyName)
		if lowered == "enabled" and instance:IsA("BaseScript") then
			return "Disabled"
		end
		if lowered == "ignoreguiinset" and instance:IsA("ScreenGui") then
			return "ScreenInsets"
		end
		local contentName = RbxDomModule.getContentPropertyAliases(instance.ClassName)[lowered]
		if contentName then
			return contentName
		end
		if instance.ClassName == "Camera" and CAMERA_FOV_PROPERTIES[lowered] then
			return "FieldOfView"
		end
		if instance:IsA("BasePart") then
			if lowered == "brickcolor" or lowered == "color3uint8" then
				return "Color"
			end
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

	local function propertyReadNameForEvent(instance: Instance, propertyName: string): string
		local className = instance.ClassName
		local names = propertyReadNamesByClass[className]
		if names == nil then
			names = {}
			propertyReadNamesByClass[className] = names
		end
		local cached = names[propertyName]
		if cached ~= nil then
			return cached
		end
		local name = uncachedPropertyReadName(instance, propertyName)
		names[propertyName] = name
		return name
	end

	local function propertyValueFingerprint(instance: Instance, propertyName: string, value: any): string
		if string.lower(propertyName) == "source" and type(value) == "string" then
			-- ScriptEditorService normalizes Windows line endings while applying Source.
			value = string.gsub(string.gsub(value, "\r\n", "\n"), "\r", "\n")
		end
		if string.lower(propertyName) == "parent" then
			-- Parent is an object identity, not its mutable name or hierarchy path.
			return if value == nil then "parent:nil" else "parent:" .. changeIdentity(value)
		end
		if typeof(value) == "Instance" then
			-- Names/moves invalidate exported paths separately; a Ref changes only
			-- when it points to a different object, even at the same path.
			return "Instance:" .. changeIdentity(value)
		end
		if typeof(value) == "Content" then
			if value.SourceType == Enum.ContentSourceType.Object then
				return "Content.Object:" .. stableValueString(value.Object)
			end
			value = BridgeContent.serialize(value)
		end
		local descriptor = RbxDomModule.findCanonicalPropertyDescriptor(instance.ClassName, propertyName)
		if descriptor ~= nil and descriptor.dataType == "Float32" and type(value) == "number" then
			buffer.writef32(FLOAT32_FINGERPRINT_BUFFER, 0, value)
			value = buffer.readf32(FLOAT32_FINGERPRINT_BUFFER, 0)
		end
		return stableValueString(value)
	end

	expectedPropertyFingerprint = function(instance: Instance, propertyName: string, value: any): string?
		local readName = propertyReadNameForEvent(instance, propertyName)
		if string.lower(readName) ~= string.lower(propertyName) then
			return nil
		end
		return propertyValueFingerprint(instance, readName, value)
	end

	local function readInstanceProperty(instance: Instance, propertyName: string): any
		if propertyName == "Scale" and (instance:IsA("Model") or instance:IsA("WorldModel")) then
			return (instance :: any):GetScale()
		end
		return (instance :: any)[propertyName]
	end

	local function readPropertyValue(instance: Instance, propertyName: string, preparedReadName: string?): (boolean, any, string)
		local readName = preparedReadName or propertyReadNameForEvent(instance, propertyName)
		local ok, value = pcall(readInstanceProperty, instance, readName)
		if not ok and readName ~= propertyName then
			ok, value = pcall(readInstanceProperty, instance, propertyName)
			readName = propertyName
		end
		return ok, value, readName
	end

	local function readPropertyFingerprint(
		instance: Instance,
		propertyName: string
	): (string?, boolean, any, boolean, any, string?)
		local lowered = string.lower(propertyName)
		if ATTRIBUTE_EVENT_PROPERTIES[lowered] then
			local attributes = instance:GetAttributes()
			return stableValueString(attributes), false, nil, true, attributes
		end
		if lowered == "parent" then
			return propertyValueFingerprint(instance, "Parent", instance.Parent),
				false,
				nil,
				true,
				instance.Parent
		end
		if lowered == "source" then
			local okSource, source = BridgeScriptDocuments.readSource(instance)
			if not okSource then
				return nil, false, nil, false, nil
			end
			local directOk, directValue = encodeDirectValue(source)
			return propertyValueFingerprint(instance, "Source", source), directOk, directValue, true, source
		end

		local okValue, value, fingerprintName = readPropertyValue(instance, propertyName)
		if not okValue then
			return nil, false, nil, false, nil
		end
		local directOk, directValue = encodeDirectValue(value)
		return propertyValueFingerprint(instance, fingerprintName, value), directOk, directValue, true, value, fingerprintName
	end

	primeExpectedProperty = function(instance: Instance, propertyName: string)
		if string.lower(propertyName) == "parent" then
			if state.parentBaselineByInstance[instance] == nil then
				state.parentBaselineByInstance[instance] = instance.Parent or NIL_PROPERTY_BASELINE
			end
			return
		end
		local key = propertyCacheKey(instance, propertyName)
		local cache = state.propertyFingerprintByInstance[instance]
		local baselines = state.propertyBaselineByInstance[instance]
		local slot = if baselines then baselines[1][key] else nil
		if cache ~= nil and cache[key] ~= nil or slot ~= nil and baselines[slot] ~= nil then
			return
		end
		local fingerprint = readPropertyFingerprint(instance, propertyName)
		if fingerprint ~= nil then
			if cache == nil then
				cache = {}
				state.propertyFingerprintByInstance[instance] = cache
			end
			cache[key] = fingerprint
		end
	end

	local function shouldRecordPropertyDirty(
		instance: Instance,
		propertyName: string
	): (boolean, boolean, any, string?, boolean, any, string?)
		if string.lower(propertyName) == "parent" then
			local parent = instance.Parent
			local baseline = parent or NIL_PROPERTY_BASELINE
			local previous = state.parentBaselineByInstance[instance] or nativeInitialParent(instance)
			state.parentBaselineByInstance[instance] = baseline
			return previous ~= baseline, false, nil,
				propertyValueFingerprint(instance, "Parent", parent), true, parent
		end
		local fingerprint, directOk, directValue, valueCaptured, value, readName = readPropertyFingerprint(instance, propertyName)
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
		local baselines = state.propertyBaselineByInstance[instance]
		local slot = if baselines then baselines[1][key] else nil
		if slot ~= nil and baselines[slot] ~= nil then
			local baseline = baselines[slot]
			baselines[slot] = nil
			previous = propertyValueFingerprint(instance, readName or propertyName,
				if baseline == NIL_PROPERTY_BASELINE then nil else baseline)
		end
		return previous == nil or previous ~= fingerprint, directOk, directValue, fingerprint, valueCaptured, value, readName
	end

	local function primeCurrentProperties(instance: Instance)
		local className = instance.ClassName
		local plan = propertyPrimingPlansByClass[className]
		if plan == nil then
			plan = { slots = {} }
			local entriesByKey = {}
			for _, name in pairs((state.propertyNamesByClass or {})[className] or {}) do
				local lowered = string.lower(name)
				if ALWAYS_IGNORED_PROPERTIES[lowered] then
					continue
				end
				local descriptor = RbxDomModule.findCanonicalPropertyDescriptor(className, name)
				-- PVInstance's inspector-only Origin/Pivot Offset fields have no Luau
				-- getter or saved value. Keep real PivotOffset/WorldPivotData baselines.
				if descriptor ~= nil and descriptor.className == "PVInstance"
					and descriptor.scriptability == "None" and descriptor.serialization == "DoesNotSerialize" then
					continue
				end
				local key = propertyCacheKey(instance, name)
				local entry = entriesByKey[key]
				if entry then
					-- Canonical aliases share a baseline, but retain their raw getter
					-- fallbacks if the canonical property is unavailable in this Studio.
					if not entry[4] then
						entry[5] = entry[5] or {}
						entry[5][#entry[5] + 1] = name
					end
				else
					entry = { name, propertyReadNameForEvent(instance, name), key,
						lowered == "parent" or lowered == "source" or ATTRIBUTE_EVENT_PROPERTIES[lowered] }
					entriesByKey[key] = entry
					plan[#plan + 1] = entry
					plan.slots[key] = #plan + 1
				end
			end
			propertyPrimingPlansByClass[className] = plan
		end
		local baselines = state.propertyBaselineByInstance[instance]
		local fingerprints = state.propertyFingerprintByInstance[instance]
		for slot, entry in ipairs(plan) do
			if entry[4] then
				shouldRecordPropertyDirty(instance, entry[1])
				fingerprints = state.propertyFingerprintByInstance[instance]
				continue
			end
			local ok, value, readName = readPropertyValue(instance, entry[1], entry[2])
			if not ok and entry[5] then
				for _, name in ipairs(entry[5]) do
					if name ~= entry[2] then
						ok, value = pcall(readInstanceProperty, instance, name)
						readName = name
						if ok then break end
					end
				end
			end
			if not ok then
				continue
			end
			-- Immutable values can wait for their first event. Capture reference
			-- identities and mutable table contents now.
			if typeof(value) == "Instance" or type(value) == "table" then
				if fingerprints == nil then
					fingerprints = {}
					state.propertyFingerprintByInstance[instance] = fingerprints
				end
				fingerprints[entry[3]] = propertyValueFingerprint(instance, readName, value)
			else
				if baselines == nil then
					baselines = table.create(#plan + 1)
					-- Keep this exact slot map if the configured schema later changes.
					baselines[1] = plan.slots
					state.propertyBaselineByInstance[instance] = baselines
				end
				baselines[slot + 1] = if value == nil then NIL_PROPERTY_BASELINE else value
			end
		end
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

	local function connectAttributeChanged(instance: Instance, serviceName: string): RBXScriptConnection
		return instance.AttributeChanged:Connect(function(attributeName: string)
			-- Export guards need raw events, including expected writes and events
			-- excluded from Live Sync. Never reuse the filtered dirty generation.
			local observation = state.attributeObservations[serviceName]
			if observation and observation.active then
				observation.generation += 1
				observation.instance = instance
				observation.attribute = attributeName
			end
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
					local details = changeDetailsForInstance(instance, "attribute", nil, attribute, "attribute changed")
					details.valueCaptured = directOk
					details.value = directValue
					details.journalValueCaptured = valueCaptured
					details.journalValue = value
					markDirty(serviceName, details)
				end
			end
		end)
	end

	promoteAttributeConnection = function(instance: Instance)
		if state.instanceConnections[instance] ~= nativeAttributeConnection then return end
		local serviceName = state.connectionServiceByInstance[instance]
		if serviceName == nil then return end
		local fingerprints = state.propertyFingerprintByInstance[instance] or {}
		for name, value in pairs(instance:GetAttributes()) do
			fingerprints["attribute:" .. name] = stableValueString(value)
		end
		state.propertyFingerprintByInstance[instance] = fingerprints
		state.instanceConnections[instance] = connectAttributeChanged(instance, serviceName)
	end

	local function promoteNativeAttributeConnections()
		if nativeAttributeRelay then
			nativeAttributeRelay.ready = false
		end
		for instance, connection in pairs(state.instanceConnections) do
			local parent = nativeInitialParent(instance)
			if parent then
				state.parentBaselineByInstance[instance] = state.parentBaselineByInstance[instance] or parent
				state.lastParentByInstance[instance] = state.lastParentByInstance[instance] or instance.Parent
			end
			if connection == nativeAttributeConnection then
				promoteAttributeConnection(instance)
			end
		end
		table.clear(nativeParentReceipts)
	end

	local function prepareNativeAttributeRelay(services: { string })
		if nativeAttributeRelay then return nativeAttributeRelay.path end
		if state.persistentTracking or state.onlyCodeMode or not state.syncbackProperties then return nil end
		local notify = Instance.new("ObjectValue")
		notify.Name = "ReniumAttributeJournal_" .. game:GetService("HttpService"):GenerateGUID(false)
		notify.Archivable = false
		local relay = { notify = notify, ready = false, nativeArmed = false, attributeGeneration = 0, path = { "CoreGui", notify.Name } }
		relay.services = {}
		for _, serviceName in ipairs(services) do
			relay.services[serviceName] = true
		end
		nativeAttributeRelay = relay
		relay.connection = notify.Changed:Connect(function(instance)
			if nativeAttributeRelay ~= relay then return end
			if instance == notify then
				relay.nativeArmed = true
				local pendingTracking = relay.pendingTracking
				relay.pendingTracking = nil
				local startPending = pendingTracking ~= nil and state.trackingGuards[pendingTracking.guardId] ~= nil
				relay.ready = not state.persistentTracking and (state.started or startPending)
				if startPending then
					ensureTracking(pendingTracking.services)
				end
				return
			elseif instance == nil then
				relay.nativeArmed = false
				-- A host timeout or cancellation restores local observation before
				-- the native subscription is disconnected under the DataModel lock.
				promoteNativeAttributeConnections()
				if relay.cached then
					verifiedPushProof = nil
					releaseProofConnections(relay)
					releaseTagConnections()
					relay.connection:Disconnect()
					relay.frameConnection:Disconnect()
					relay.notify:Destroy()
					nativeAttributeRelay = nil
				end
				return
			end
			relay.attributeGeneration += 1
			if relay.cached then return end
			local connection = state.instanceConnections[instance]
			if connection ~= nil and connection ~= nativeAttributeConnection then return end
			local journal = state.changeJournal
			if connection == nil and (journal == nil or journal.nativeAdditions == nil) then return end
			local serviceName = state.connectionServiceByInstance[instance] or serviceNameForTrackedInstance(instance)
			if serviceName == nil or shouldIgnoreInstance(instance, serviceName) then return end
			local observation = state.attributeObservations[serviceName]
			if observation and observation.active then
				observation.generation += 1
				observation.instance = instance
				observation.attribute = "*"
			end
			-- Initial binary attributes were loaded before parenting. An Attributes
			-- event on a new, attached object is an outside edit. This also catches
			-- another plugin's callback preceding our DescendantAdded callback.
			markDirty(serviceName, {
				instance = instance, action = "attributes", reason = "attributes changed",
				journalValueCaptured = true, journalValue = instance:GetAttributes(),
			})
		end)
		local frame = 0
		relay.frameConnection = game:GetService("RunService").Heartbeat:Connect(function()
			if relay.nativeArmed then
				frame += 1
				notify:SetAttribute("Frame", frame)
			end
		end)
		notify.Parent = game:GetService("CoreGui")
		return relay.path
	end

	local function disconnectInstance(instance: Instance, expectedServiceName: string?)
		if
			expectedServiceName ~= nil
			and state.connectionServiceByInstance[instance] ~= expectedServiceName
		then
			return
		end
		local attributeConnection = state.instanceConnections[instance]
		local observation = state.attributeObservations[state.connectionServiceByInstance[instance]]
		if observation and attributeConnection then
			observation.active = false
		end
		state.instanceConnections[instance] = nil
		state.connectionServiceByInstance[instance] = nil
		state.tagFingerprintByInstance[instance] = nil
		state.propertyFingerprintByInstance[instance] = nil
		state.parentBaselineByInstance[instance] = nil
		state.propertyBaselineByInstance[instance] = nil
		state.lastParentByInstance[instance] = nil
		if attributeConnection then
			state.connectedInstanceCount = math.max(0, state.connectedInstanceCount - 1)
			attributeConnection:Disconnect()
		end
		for _, connection in ipairs(state.fallbackPropertyConnections[instance] or {}) do
			connection:Disconnect()
		end
		state.fallbackPropertyConnections[instance] = nil
	end

	local function disconnectInstanceTree(instance: Instance, expectedServiceName: string?)
		for _, descendant in ipairs(instance:GetDescendants()) do
			disconnectInstance(descendant, expectedServiceName)
		end
		disconnectInstance(instance, expectedServiceName)
	end

	local function invalidateParentOrdinals(instance: Instance)
		local parent = instance.Parent
		local previous = state.lastParentByInstance[instance] or nativeInitialParent(instance)
		invalidateSiblingOrdinals(previous)
		invalidateSiblingOrdinals(parent)
		state.lastParentByInstance[instance] = parent
	end

	local function recordInstancePropertyChange(instance: Instance, serviceName: string, property: string, sampleOnly: boolean?)
		property = RbxDomModule.getContentPropertyAliases(instance.ClassName)[string.lower(property)] or property
		if string.lower(property) == "enabled" and instance:IsA("BaseScript") then
			property = "Disabled"
		end
		if string.lower(property) == "ignoreguiinset" and instance:IsA("ScreenGui") then
			property = "ScreenInsets"
		end
		local lowered = string.lower(property)
		if lowered == "name" then
			invalidateSiblingOrdinals(instance.Parent)
		end
		local shouldRecord, directOk, directValue, fingerprint, valueCaptured, value, readName =
			shouldRecordPropertyDirty(instance, property)
		-- A failed read is not an observed edit. Real engine events still record
		-- unreadable properties, but our post-setter sample must not invent one.
		if sampleOnly and not valueCaptured then
			return
		end
		if consumeExpectedInstanceEvent(state.expectedInstanceProperties, instance, lowered, fingerprint, nil) then
			return
		end
		if readName ~= nil and readName ~= property then
			property = readName
			-- An alias can signal before its canonical field. Match the canonical
			-- expectation and journal the name belonging to the captured value.
			if string.lower(property) ~= lowered
				and consumeExpectedInstanceEvent(state.expectedInstanceProperties, instance, string.lower(property), fingerprint, nil) then
				return
			end
		end
		if not shouldRecord then
			return
		end
		if not markDirectProperty(instance, serviceName, property, directOk, directValue, valueCaptured, value) then
			local details = changeDetailsForInstance(instance, "property", property, nil, "property changed")
			details.journalValueCaptured = valueCaptured
			details.journalValue = value
			markDirty(serviceName, details)
		end
	end

	function api.samplePropertyChange(instance: Instance, propertyName: string)
		local serviceName = state.connectionServiceByInstance[instance]
		if serviceName ~= nil and isRelevantInstanceProperty(instance, propertyName) then
			recordInstancePropertyChange(instance, serviceName, propertyName, true)
		end
	end

	local function observePropertyChange(instance: Instance, serviceName: string, propertyName: string)
		if string.lower(propertyName) == "archivable" then
			updateTrackedArchivable(instance, serviceName)
		end
		if propertyName == "Parent" then
			invalidateParentOrdinals(instance)
		end
		-- CornerRadius aliases TopLeftRadius; record one canonical value.
		local property = if instance.ClassName == "UICorner" and propertyName == "CornerRadius"
			then "TopLeftRadius"
			else if instance.ClassName == "Camera" and CAMERA_FOV_PROPERTIES[string.lower(propertyName)] then "FieldOfView"
			else if (propertyName == "BrickColor" or propertyName == "Color3uint8") and instance:IsA("BasePart") then "Color"
			else propertyName
		if not isRelevantInstanceProperty(instance, property) then
			return
		end
		recordInstancePropertyChange(instance, serviceName, property)
		if instance.ClassName == "MeshPart" and property == "CollisionFidelity" then
			-- Box only signals before its value changes. Keep the immediate conflict
			-- sample, then read after the setter without polling other properties.
			task.defer(function()
				if state.connectionServiceByInstance[instance] == serviceName then
					recordInstancePropertyChange(instance, serviceName, property)
				end
			end)
		end
	end

	local function connectPropertyChanges(instance: Instance, serviceName: string, refresh: boolean?)
		local connections = state.fallbackPropertyConnections[instance]
		if connections ~= nil then
			if not refresh then return end
			for _, connection in ipairs(connections) do
				connection:Disconnect()
			end
			table.clear(connections)
		else
			connections = {}
			state.fallbackPropertyConnections[instance] = connections
		end
		local isValueBase = instance:IsA("ValueBase")
		-- Destroying a script-containing ValueBase can also fire its Changed
		-- signal without changing Value. Subscribe to the property itself.
		local signal = if isValueBase then instance:GetPropertyChangedSignal("Value") else instance.Changed
		connections[#connections + 1] = signal:Connect(function(propertyName: any)
			observePropertyChange(instance, serviceName, if isValueBase then "Value" else tostring(propertyName))
		end)
		if isValueBase then
			-- ValueBase.Changed only fires for Value. Detached objects also need
			-- ordinary property observations: ItemChanged cannot see them there.
			local className = instance.ClassName
			local names = valuePropertySignalNamesByClass[className]
			if names == nil then
				names = { "Name", "Parent", "Archivable" }
				local seen = { name = true, parent = true, archivable = true, value = true }
				for key, name in pairs((state.propertyNamesByClass or {})[className] or {}) do
					local descriptor = RbxDomModule.findCanonicalPropertyDescriptor(className, name)
					if not seen[key] and descriptor ~= nil
						and (descriptor.scriptability == "ReadWrite" or descriptor.scriptability == "Read") then
						-- Reflection can describe a property absent from this Studio.
						local ok = pcall(instance.GetPropertyChangedSignal, instance, name)
						if ok then
							names[#names + 1] = name
						end
					end
				end
				valuePropertySignalNamesByClass[className] = names
			end
			for _, name in ipairs(names) do
				connections[#connections + 1] = instance:GetPropertyChangedSignal(name):Connect(function()
					observePropertyChange(instance, serviceName, name)
				end)
			end
		end
	end

	local function refreshValuePropertySignals(previousCandidates: PropertyNameSetByClass?)
		local changedClasses = {}
		for className in pairs(valuePropertySignalNamesByClass) do
			local previous = if previousCandidates then previousCandidates[className] else nil
			local current = if state.propertyNamesByClass then state.propertyNamesByClass[className] else nil
			if not expectedValuesEqual(previous, current) then
				changedClasses[className] = true
				valuePropertySignalNamesByClass[className] = nil
			end
		end
		if next(changedClasses) == nil then return end
		-- A schema can change while a transaction retains detached observers.
		-- Refresh only affected ValueBase classes, without resetting their journal.
		for instance in pairs(state.fallbackPropertyConnections) do
			if changedClasses[instance.ClassName] then
				connectPropertyChanges(instance, state.connectionServiceByInstance[instance], true)
			end
		end
	end

	local function releaseDetachedObserver(instance: Instance, serviceName: string)
		local journal = state.changeJournal
		if journal ~= nil then
			journal.detachedInstances[instance] = nil
		end
		if state.itemChangedAvailable and state.connectionServiceByInstance[instance] == serviceName then
			for _, connection in ipairs(state.fallbackPropertyConnections[instance] or {}) do
				connection:Disconnect()
			end
			state.fallbackPropertyConnections[instance] = nil
		end
	end

	local function retainDetachedObserver(instance: Instance, serviceName: string)
		local journal = state.changeJournal
		if journal == nil or not journal.services[serviceName]
			or state.connectionServiceByInstance[instance] ~= serviceName then return end
		journal.detachedInstances[instance] = serviceName
		promoteAttributeConnection(instance)
		-- DataModel.ItemChanged stops observing an object outside the DataModel.
		-- Keep the local signal only for the detached transaction interval.
		connectPropertyChanges(instance, serviceName)
	end

	releaseJournalObservers = function(journal)
		for instance, serviceName in pairs(journal.detachedInstances) do
			if state.connectionServiceByInstance[instance] == serviceName then
				local service = state.serviceRoots[serviceName]
				if service ~= nil and instance:IsDescendantOf(service) then
					releaseDetachedObserver(instance, serviceName)
				else
					disconnectInstance(instance, serviceName)
				end
			end
		end
	end

	local function connectInstance(instance: Instance, serviceName: string, primeCurrentValues: boolean?, profile: { [string]: number }?, nativeInsertion: boolean?, serializedInsertion: boolean?)
		local connectedServiceName = state.connectionServiceByInstance[instance]
		if state.instanceConnections[instance] ~= nil then
			if connectedServiceName == serviceName then
				return
			end
			disconnectInstance(instance, connectedServiceName)
		end
		if not nativeInsertion and shouldIgnoreInstance(instance, serviceName) then
			return
		end
		local nativeAttributes = nativeAttributeRelay ~= nil and nativeAttributeRelay.ready
			and nativeAttributeRelay.services[serviceName] == true
		state.connectionServiceByInstance[instance] = serviceName
		local journal = state.changeJournal
		if nativeInsertion and nativeAttributes and state.itemChangedAvailable then
			-- Shared property/attribute signals are already armed. No tag scan or
			-- duplicate parent maps are needed for a proved untagged native class.
			if not (journal and journal.nativeUntaggedClasses and journal.nativeUntaggedClasses[instance.ClassName]) then
				local tags = tagFingerprint(instance)
				state.tagFingerprintByInstance[instance] = if tags == "" then nil else tags
			end
			state.instanceConnections[instance] = nativeAttributeConnection
			state.connectedInstanceCount += 1
			if profile then
				profile.nativeAttributeInstances = (profile.nativeAttributeInstances or 0) + 1
				profile.receiptBaselineInstances = (profile.receiptBaselineInstances or 0) + 1
				profile.connectedInstances = (profile.connectedInstances or 0) + 1
			end
			return
		end
		local tags = tagFingerprint(instance)
		state.tagFingerprintByInstance[instance] = if tags == "" then nil else tags

		local phaseStarted = if profile then os.clock() else 0
		if primeCurrentValues or state.expectedFreshInstances[instance] then
			-- The native loader sets ordinary values before exposing its objects.
			-- Keep Parent for delayed insertion events. A property or attribute
			-- event without a baseline remains a change, never a no-op.
			if nativeInsertion or serializedInsertion then
				-- Parent already is an identity; do not allocate a property table
				-- and stringify it for every object in a full replacement.
				state.parentBaselineByInstance[instance] = instance.Parent or NIL_PROPERTY_BASELINE
			else
				shouldRecordPropertyDirty(instance, "Parent")
				primeCurrentProperties(instance)
			end
			if profile then
				local now = os.clock()
				local elapsed = (now - phaseStarted) * 1000
				profile.baselineMs = (profile.baselineMs or 0) + elapsed
				local key = `baseline:{instance.ClassName}`
				profile[key] = (profile[key] or 0) + elapsed
				phaseStarted = now
				profile.primedInstances = (profile.primedInstances or 0) + 1
			end
			local fingerprints = state.propertyFingerprintByInstance[instance]
			if not nativeAttributes and not serializedInsertion then
				-- A deserialized payload finished its attribute writes before these
				-- listeners existed. Capture later edits at event time; copying every
				-- initial attribute map adds no protection against outside writes.
				for attributeName, value in pairs(instance:GetAttributes()) do
					if fingerprints == nil then
						fingerprints = {}
						state.propertyFingerprintByInstance[instance] = fingerprints
					end
					fingerprints["attribute:" .. attributeName] = stableValueString(value)
				end
			end
			if profile then
				local now = os.clock()
				profile.attributeBaselineMs = (profile.attributeBaselineMs or 0) + (now - phaseStarted) * 1000
				phaseStarted = now
			end
		end

		state.lastParentByInstance[instance] = instance.Parent
		if not state.itemChangedAvailable then
			connectPropertyChanges(instance, serviceName)
		end

		state.instanceConnections[instance] = if nativeAttributes then nativeAttributeConnection
			else connectAttributeChanged(instance, serviceName)
		state.connectedInstanceCount += 1
		if profile then
			if nativeAttributes then
				profile.nativeAttributeInstances = (profile.nativeAttributeInstances or 0) + 1
			end
			profile.listenerSetupMs = (profile.listenerSetupMs or 0) + (os.clock() - phaseStarted) * 1000
			profile.connectedInstances = (profile.connectedInstances or 0) + 1
		end
	end

	prepareParentObservers = function(instances: { Instance }, nextParent: Instance?, token: any, profile: { [string]: number }?, serializedInsertion: boolean?)
		local serviceName = serviceNameForTrackedInstance(nextParent or instances[1])
		if serviceName == nil then
			return
		end
		local acquired = {}
		token.complete = function(cancelled: boolean?)
			if cancelled then
				for instance, connection in pairs(acquired) do
					if (state.instanceConnections[instance] or false) == connection
						and not instance:IsDescendantOf(game) then
						if state.changeJournal ~= nil then
							state.changeJournal.detachedInstances[instance] = nil
						end
						disconnectInstance(instance, serviceName)
					end
				end
			end
			table.clear(acquired)
			token.complete = nil
		end
		-- The parent setter can run other plugins' callbacks before DescendantAdded
		-- reaches ours. Observe the detached values before exposing the subtree.
		local ok, result = pcall(function()
			for _, instance in ipairs(instances) do
				if nextParent ~= nil and state.instanceConnections[instance] == nil then
					acquired[instance] = false
					-- A detached native payload has finished setting its properties.
					-- Keep all observers and Parent/attribute baselines; ordinary
					-- property events without a baseline remain outside edits.
					connectInstance(instance, serviceName, true, profile, nil, serializedInsertion)
					acquired[instance] = state.instanceConnections[instance] or false
				end
				-- Serialized children are exposed by the immediately following root
				-- Parent setter. ItemChanged observes them before DescendantAdded;
				-- any actual removal arms the detached fallback before they leave.
				-- Keep the root fallback and all ordinary/deferred preparation paths.
				if not serializedInsertion or not state.itemChangedAvailable or nextParent == nil or instance == instances[1] then
					retainDetachedObserver(instance, serviceName)
				end
			end
		end)
		if not ok then
			api.cancelExpectedEvent(token)
			error(result, 0)
		end
	end

	local function connectExistingDescendants(descendants: { Instance }, serviceName: string)
		for _, descendant in ipairs(descendants) do
			if not state.onlyCodeMode or hasLuaSourceDescendant(descendant) then
				connectInstance(descendant, serviceName)
			end
		end
	end

	local function ensureServiceSignals(serviceName: string)
		local signals = serviceSignals[serviceName]
		if signals ~= nil then return signals end
		signals = { connections = {} }
		serviceSignals[serviceName] = signals
		local service = game:GetService(serviceName)
		signals.connections[1] = service.DescendantAdded:Connect(function(instance)
			local included = if exportStructureObserver then exportStructureObserver(serviceName, instance) else nil
			if signals.added then
				signals.added(instance, included)
			end
		end)
		signals.connections[2] = service.DescendantRemoving:Connect(function(instance)
			if exportStructureObserver then
				exportStructureObserver(serviceName, instance)
			end
			if signals.removing then
				signals.removing(instance)
			end
		end)
		return signals
	end

	local function releaseServiceSignals(serviceName: string)
		local signals = serviceSignals[serviceName]
		if signals == nil then return end
		signals.added = nil
		signals.removing = nil
		if exportStructureObserver == nil then
			for _, connection in ipairs(signals.connections) do
				connection:Disconnect()
			end
			serviceSignals[serviceName] = nil
		end
	end

	local function ensurePropertySignal(): boolean
		if propertySignal ~= nil then return true end
		local itemChanged = (game :: any).ItemChanged
		if itemChanged == nil then return false end
		propertySignal = itemChanged:Connect(function(instance: Instance, propertyName: any)
			local profile = config.syncProfile and (config.syncProfile.attachment or config.syncProfile.nativeImport)
			local started = if profile then os.clock() else 0
			if exportPropertyObserver then
				exportPropertyObserver(instance, propertyName)
			end
			if state.itemChangedAvailable and typeof(instance) == "Instance" then
				local property = string.lower(tostring(propertyName or ""))
				local trackedService = state.connectionServiceByInstance[instance]
				if trackedService ~= nil then
					observePropertyChange(instance, trackedService, tostring(propertyName))
				elseif property == "archivable" then
					local serviceName = serviceNameForTrackedInstance(instance)
					if serviceName ~= nil then
						updateTrackedArchivable(instance, serviceName)
					end
				end
				if property == "tags" then
					markTagChange(instance, "Tags", true)
				end
			end
			if profile then
				local elapsed = (os.clock() - started) * 1000
				profile.globalPropertyCallbacksMs = (profile.globalPropertyCallbacksMs or 0) + elapsed
				profile.globalPropertyCallbacks = (profile.globalPropertyCallbacks or 0) + 1
				local key = `propertyCallback:{tostring(propertyName)}`
				profile[key] = (profile[key] or 0) + elapsed
			end
		end)
		return true
	end

	-- Export invalidation remains active when Live Sync/transaction tracking stops.
	-- Sharing the engine subscriptions avoids dispatching each event into Lua twice.
	function api.observeExports(structure: (string, Instance) -> boolean?, property: (Instance, any) -> ())
		exportStructureObserver = structure
		exportPropertyObserver = property
		for serviceName in pairs(allowedServices) do
			ensureServiceSignals(serviceName)
		end
		ensurePropertySignal()
	end

	local function reconcileServiceConnections(descendants: { Instance }, serviceName: string)
		local desired = {}
		for _, descendant in ipairs(descendants) do
			if
				not shouldIgnoreInstance(descendant, serviceName)
				and (not state.onlyCodeMode or hasLuaSourceDescendant(descendant))
			then
				desired[descendant] = true
				connectInstance(descendant, serviceName)
			end
		end
		local disconnect = {}
		for instance in pairs(state.instanceConnections) do
			if state.connectionServiceByInstance[instance] == serviceName and not desired[instance] then
				table.insert(disconnect, instance)
			end
		end
		for _, instance in ipairs(disconnect) do
			disconnectInstance(instance, serviceName)
		end
	end

	local function reconcileAncestorConnections(instance: Instance, service: Instance, serviceName: string, profile: { [string]: number }?)
		local current = instance.Parent
		while current ~= nil and current ~= service and current:IsDescendantOf(service) do
			-- Full-mode connections already cover the connected ancestor's chain.
			-- Code-only mode must still reconsider every ancestor's source count.
			if not state.onlyCodeMode and state.connectionServiceByInstance[current] == serviceName then
				break
			end
			if not state.onlyCodeMode or hasLuaSourceDescendant(current) then
				connectInstance(current, serviceName, nil, profile)
			else
				disconnectInstance(current, serviceName)
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
		local tags = tagFingerprint(service)
		state.tagFingerprintByInstance[service] = if tags == "" then nil else tags
		local descendants = service:GetDescendants()
		if state.onlyCodeMode then
			rebuildLuaSourceCounts(service, descendants)
		end
		rebuildExportInstances(service, serviceName, descendants)

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
						markDirty(serviceName, details)
					end
				end
			end),
		}
		local signals = ensureServiceSignals(serviceName)
		signals.added = function(instance: Instance, exportIncluded: boolean?)
			local journal = state.changeJournal
			local nativeProfile = if journal and journal.services[serviceName] then journal.nativeProfile else nil
			local activeProfile = (config.syncProfile and config.syncProfile.attachment) or nativeProfile
			local started = if activeProfile then os.clock() else 0
			state.exportInstancesByService[serviceName] = nil
			updateTrackedArchivable(instance, serviceName, exportIncluded)
			local additions = if journal and journal.nativeAdditions then journal.nativeAdditions[serviceName] else nil
			local included = not shouldIgnoreInstance(instance, serviceName, exportIncluded)
			if additions ~= nil and included and not state.onlyCodeMode
				and additions[instance] == nil and state.connectionServiceByInstance[instance] == nil
				and state.expectedStructuralByInstance[instance] == nil then
				-- Bulk replacement is not a sequence of incremental edits. Keep
				-- only its identity/parent receipt and arm future edit signals.
				-- Reattachments and pre-observed objects take the normal path.
				local parent = instance.Parent
				if parent ~= nil and instance:IsDescendantOf(service) then
					additions[instance] = parent
					invalidateSiblingOrdinals(parent)
					connectInstance(instance, serviceName, true, nativeProfile, true)
					if activeProfile then
						activeProfile.trackerAddedCallbacksMs = (activeProfile.trackerAddedCallbacksMs or 0) + (os.clock() - started) * 1000
						activeProfile.trackerAddedCallbacks = (activeProfile.trackerAddedCallbacks or 0) + 1
					end
					return
				end
			end
			local expected, expectation = consumeExpectedInstanceEvent(
				state.expectedStructuralByInstance,
				instance,
				"added",
				nil,
				instance.Parent
			)
			local profile = if expectation then expectation.profile else nativeProfile
			if included and not expected and additions ~= nil and additions[instance] == nil and instance.Parent ~= nil then
				-- Only the first addition waits for the factory receipt. A
				-- second add/remove follows the ordinary journal immediately;
				-- property and attribute listeners are never suppressed here.
				additions[instance] = instance.Parent
				expected = true
			end
			local callbackStarted = if profile then os.clock() else 0
			if activeProfile then
				activeProfile.trackerPreExpectationMs = (activeProfile.trackerPreExpectationMs or 0) + (os.clock() - started) * 1000
			end
			if state.onlyCodeMode then
				if isLuaSourceInstance(instance) then
					adjustLuaSourceAncestors(instance, 1)
				elseif state.luaSourceDescendantCounts[instance] == nil then
					state.luaSourceDescendantCounts[instance] = 0
				end
			end
			if expectation == nil or not expectation.parentUnchanged then
				invalidateSiblingOrdinals(instance.Parent)
			end
			if
				included
				and (not state.onlyCodeMode or hasLuaSourceDescendant(instance))
			then
				if instance:IsDescendantOf(service) then
					connectInstance(instance, serviceName, true, profile, additions ~= nil and additions[instance] ~= nil)
					releaseDetachedObserver(instance, serviceName)
					reconcileAncestorConnections(instance, service, serviceName, profile)
				end
				if not expected then
					markDirty(
						serviceName,
						changeDetailsForInstance(instance, "added", nil, nil, "descendant added")
					)
				end
			end
			if profile then
				profile.postExpectationCallbackMs = (profile.postExpectationCallbackMs or 0) + (os.clock() - callbackStarted) * 1000
			end
			if activeProfile then
				activeProfile.trackerAddedCallbacksMs = (activeProfile.trackerAddedCallbacksMs or 0) + (os.clock() - started) * 1000
				activeProfile.trackerAddedCallbacks = (activeProfile.trackerAddedCallbacks or 0) + 1
			end
		end
		signals.removing = function(instance: Instance)
			state.exportInstancesByService[serviceName] = nil
			retainDetachedObserver(instance, serviceName)
			local removingParent = instance.Parent
			removeTrackedArchivable(instance, serviceName)
			local expected = consumeExpectedInstanceEvent(
				state.expectedStructuralByInstance,
				instance,
				"removed",
				nil,
				instance.Parent
			)
			local wasCodeRelevant = not state.onlyCodeMode or hasLuaSourceDescendant(instance)
			if state.onlyCodeMode and isLuaSourceInstance(instance) then
				adjustLuaSourceAncestors(instance, -1)
			end
			local ancestors = {}
			if state.onlyCodeMode then
				local current = instance.Parent
				while current ~= nil and current ~= service do
					table.insert(ancestors, current)
					current = current.Parent
				end
			end
			invalidateSiblingOrdinals(instance.Parent)
			if not expected and not shouldIgnoreInstance(instance, serviceName) and wasCodeRelevant then
				markDirty(
					serviceName,
					changeDetailsForInstance(instance, "removed", nil, nil, "descendant removing")
				)
			end
			-- The journal releases its detached observers together at completion.
			-- Their local Parent signal still invalidates the old parent's ordinals.
			if not (state.changeJournal and state.changeJournal.detachedInstances[instance]) then
				task.defer(function()
					invalidateSiblingOrdinals(removingParent)
					if
						state.connectionServiceByInstance[instance] == serviceName
						and not instance:IsDescendantOf(service)
						and not (state.changeJournal and state.changeJournal.detachedInstances[instance])
					then
						disconnectInstanceTree(instance, serviceName)
					end
				end)
			end
			if state.onlyCodeMode and #ancestors > 0 then
				task.defer(function()
					for _, ancestor in ipairs(ancestors) do
						if ancestor:IsDescendantOf(service) and not hasLuaSourceDescendant(ancestor) then
							disconnectInstance(ancestor, serviceName)
						end
					end
				end)
			end
		end

		table.insert(connections, connectAttributeChanged(service, serviceName))
		state.rootConnections[serviceName] = connections
		connectExistingDescendants(descendants, serviceName)
	end

	local function endObservationEpoch(serviceName: string)
		local observation = state.attributeObservations[serviceName]
		if observation then
			observation.active = false
			state.attributeObservations[serviceName] = nil
		end
		-- Changes can happen while this service has no listeners. Advance the
		-- generation so snapshots proven in the old epoch cannot be reused later.
		state.seq += 1
		state.mutationSeqByService[serviceName] = state.seq
		state.checkpointSeqByService[serviceName] = state.seq
		config.invalidateRuntimeExportCache(serviceName)
	end

	local function unwatchService(serviceName: string, preservePending: boolean?)
		local service = state.serviceRoots[serviceName]
		if service == nil then
			return
		end
		endObservationEpoch(serviceName)
		releaseServiceSignals(serviceName)
		for _, connection in ipairs(state.rootConnections[serviceName] or {}) do
			connection:Disconnect()
		end
		state.rootConnections[serviceName] = nil
		local disconnect = {}
		for instance in pairs(state.instanceConnections) do
			if state.connectionServiceByInstance[instance] == serviceName then
				table.insert(disconnect, instance)
			end
		end
		for _, instance in ipairs(disconnect) do
			disconnectInstance(instance, serviceName)
		end
		for _, instance in ipairs(service:GetDescendants()) do
			state.archivableByInstance[instance] = nil
		end
		state.nonArchivableCountByService[serviceName] = nil
		state.exportInstancesByService[serviceName] = nil
		state.watchedServices[serviceName] = nil
		state.serviceRoots[serviceName] = nil
		state.serviceNameByRoot[service] = nil
		if not preservePending then
			state.dirtySeqByService[serviceName] = nil
			clearRestoredPendingService(serviceName)
			state.fullSyncSeqByService[serviceName] = nil
			clearPropertyChangesForService(serviceName)
			clearChangeLogsForService(serviceName)
			persistPendingServices()
		end
	end

	local function stopTracking()
		if nativeTerrainRelay then
			nativeTerrainRelay.connection:Disconnect()
			nativeTerrainRelay.notify:Destroy()
			nativeTerrainRelay = nil
		end
		releaseLocalPushObservation()
		localPushProofRequested = false
		state.started = false
		table.clear(nativeParentReceipts)
		if config.syncProfile then
			config.syncProfile.nativeImport = nil
		end
		if nativeAttributeRelay and nativeAttributeRelay.cached then
			nativeAttributeRelay.ready = false
		elseif nativeAttributeRelay then
			releaseProofConnections(nativeAttributeRelay)
			nativeAttributeRelay.connection:Disconnect()
			nativeAttributeRelay.frameConnection:Disconnect()
			nativeAttributeRelay.notify:Destroy()
			nativeAttributeRelay = nil
		end
		for serviceName in pairs(state.watchedServices) do
			endObservationEpoch(serviceName)
			releaseServiceSignals(serviceName)
		end
		for _, connections in pairs(state.rootConnections) do
			for _, connection in ipairs(connections) do
				connection:Disconnect()
			end
		end
		for _, connection in pairs(state.instanceConnections) do
			connection:Disconnect()
		end
		for _, connections in pairs(state.fallbackPropertyConnections) do
			for _, connection in ipairs(connections) do
				connection:Disconnect()
			end
		end
		table.clear(state.rootConnections)
		table.clear(state.instanceConnections)
		table.clear(state.fallbackPropertyConnections)
		table.clear(state.connectionServiceByInstance)
		table.clear(state.tagFingerprintByInstance)
		table.clear(state.propertyFingerprintByInstance)
		table.clear(state.parentBaselineByInstance)
		table.clear(state.propertyBaselineByInstance)
		table.clear(state.ordinalCacheByParent)
		table.clear(state.lastParentByInstance)
		table.clear(state.expectedFreshInstances)
		table.clear(state.watchedServices)
		table.clear(state.serviceRoots)
		table.clear(state.serviceNameByRoot)
		table.clear(state.luaSourceDescendantCounts)
		table.clear(state.archivableByInstance)
		table.clear(state.nonArchivableCountByService)
		table.clear(state.exportInstancesByService)
		state.connectedInstanceCount = 0
		if exportPropertyObserver == nil and propertySignal ~= nil then
			propertySignal:Disconnect()
			propertySignal = nil
		end
		if not (nativeAttributeRelay and nativeAttributeRelay.cached) then
			releaseTagConnections()
		end
		state.itemChangedAvailable = false
		signalChange()
	end

	local function releaseTrackingGuard(rawGuardId: any)
		if type(rawGuardId) ~= "string" or rawGuardId == "" then
			return
		end
		state.trackingGuards[rawGuardId] = nil
		if not state.persistentTracking and next(state.trackingGuards) == nil then
			stopTracking()
		end
	end

	local function acquireTrackingGuard(guardId: string, duration: number?)
		local lifetime = duration or TRACKING_GUARD_TTL_SECONDS
		local expiresAt = os.clock() + lifetime
		state.trackingGuards[guardId] = expiresAt
		task.delay(lifetime, function()
			if state.trackingGuards[guardId] ~= expiresAt then
				return
			end
			state.trackingGuards[guardId] = nil
			if localPushObservation ~= nil and localPushObservation.guardId == guardId then
				releaseLocalPushObservation()
			end
			if not state.persistentTracking and next(state.trackingGuards) == nil then
				stopTracking()
			end
		end)
	end

	ensureTracking = function(services: { string })
		if config.bridgeRole ~= "edit" then
			return
		end
		if not state.started then
			state.itemChangedAvailable = ensurePropertySignal()
		end
		for _, serviceName in ipairs(services) do
			ensureService(serviceName)
		end
		if not state.started then
			observeTagDiscovery()
			discoverTags(false)
			state.started = true
		end
	end

	function api.configurePropertyCandidates(rawCandidatesByClass: any): { [string]: any }
		local previousCandidates = state.propertyNamesByClass
		table.clear(propertyEventRelevanceByClass)
		table.clear(propertyReadNamesByClass)
		table.clear(propertyCacheKeysByClass)
		table.clear(propertyPrimingPlansByClass)
		if type(rawCandidatesByClass) ~= "table" then
			state.propertyNamesByClass = nil
			state.propertyFilterClassCount = 0
			state.propertyFilterPropertyCount = 0
			refreshValuePropertySignals(previousCandidates)
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
		refreshValuePropertySignals(previousCandidates)
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
			table.clear(state.luaSourceDescendantCounts)
			for serviceName, service in pairs(state.serviceRoots) do
				local descendants = service:GetDescendants()
				if state.onlyCodeMode then
					rebuildLuaSourceCounts(service, descendants)
				end
				reconcileServiceConnections(descendants, serviceName)
				if not state.onlyCodeMode then
					rebuildExportInstances(service, serviceName, descendants)
				end
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
		local affectedServices = {}
		for _, serviceName in ipairs(raw.services or {}) do
			serviceName = tostring(serviceName)
			if allowedServices[serviceName] then
				affectedServices[serviceName] = true
			end
		end
		local function addExpected(target: { [string]: ExpectedValueQueue }, key: string, value: any)
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
				affectedServices[serviceName] = true
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
				affectedServices[serviceName] = true
				for propertyName, value in pairs(change.properties or {}) do
					addExpected(
						state.expectedProperties,
						expectedPathKey(serviceName, change.pathSegments, change.pathOrdinals, tostring(propertyName)),
						value
					)
				end
				for attributeName, value in pairs(change.attributes or {}) do
					addExpected(
						state.expectedAttributes,
						expectedPathKey(serviceName, change.pathSegments, change.pathOrdinals, tostring(attributeName)),
						value
					)
				end
				for _, attributeName in ipairs(change.deletedAttributes or {}) do
					addExpected(
						state.expectedAttributes,
						expectedPathKey(serviceName, change.pathSegments, change.pathOrdinals, tostring(attributeName)),
						nil
					)
				end
			end
		end
		for serviceName in pairs(affectedServices) do
			state.seq += 1
			state.checkpointSeqByService[serviceName] = state.seq
			config.invalidateRuntimeExportCache(serviceName)
		end
	end

	local function clearExpectedEvents()
		table.clear(state.expectedProperties)
		table.clear(state.expectedAttributes)
		table.clear(state.expectedStructuralByInstance)
		table.clear(state.expectedInstanceProperties)
		table.clear(state.expectedInstanceAttributes)
		table.clear(state.expectedTags)
		table.clear(state.expectedFreshInstances)
	end

	function api.beginSuppress(seconds: number?, expectation: any)
		if state.suppressDepth == 0 then
			state.expectedGeneration += 1
			-- Deferred Roblox signals from the previous mutation can arrive after its
			-- bridge request has returned. Keep those exact expectations while the
			-- previous settle window is active so a back-to-back transaction cannot
			-- mistake Renium's own event for an external Studio edit.
			if os.clock() >= state.suppressUntil then
				clearExpectedEvents()
			end
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
		pendingView = nil
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
					clearRestoredPendingService(serviceName)
				end
				local fullSyncSeq = state.fullSyncSeqByService[serviceName]
				if fullSyncSeq ~= nil and fullSyncSeq <= ackSeq then
					state.fullSyncSeqByService[serviceName] = nil
				end
			end
			for key, change in pairs(state.propertyChangesByKey) do
				if requested[change.service] and change.seq <= ackSeq then
					state.directPropertyBytes = math.max(0, state.directPropertyBytes - change.estimatedBytes)
					state.directPropertyCount = math.max(0, state.directPropertyCount - 1)
					state.propertyChangesByKey[key] = nil
				end
			end
			for key, change in pairs(state.changeLogByKey) do
				if requested[change.service] and change.seq <= ackSeq then
					state.changeLogByKey[key] = nil
					state.changeLogCountByService[change.service] =
						math.max(0, (state.changeLogCountByService[change.service] or 0) - 1)
				elseif requested[change.service] and change.action == "added" and (change.firstSeq or change.seq) <= ackSeq then
					-- The original addition was published even if this object was moved
					-- again later. It is no longer a never-synced temporary object.
					change.firstSeq = 0
				end
			end
			persistPendingServices()
			signalTrackedChange()
		end
		if params.clearPending == true or params.reset == true then
			for _, serviceName in ipairs(services) do
				state.dirtySeqByService[serviceName] = nil
				clearRestoredPendingService(serviceName)
				state.fullSyncSeqByService[serviceName] = nil
				clearPropertyChangesForService(serviceName)
				clearChangeLogsForService(serviceName)
			end
			persistPendingServices()
		end
		local clearRestoredPendingEpoch = params.clearRestoredPendingEpoch
		if type(clearRestoredPendingEpoch) == "string"
			and clearRestoredPendingEpoch ~= ""
			and clearRestoredPendingEpoch == state.restoredPendingEpoch
		then
			local changed = false
			for _, serviceName in ipairs(services) do
				if state.restoredPendingServices[serviceName] then
					clearRestoredPendingService(serviceName)
					state.dirtySeqByService[serviceName] = nil
					state.fullSyncSeqByService[serviceName] = nil
					clearPropertyChangesForService(serviceName)
					clearChangeLogsForService(serviceName)
					changed = true
				end
			end
			if changed then
				persistPendingServices()
			end
		end
	end

	local function buildStateResponse(services: { string }, compact: boolean): { [string]: any }
		local visible = pendingChanges()
		local requested = {}
		for _, serviceName in ipairs(services) do
			requested[serviceName] = true
		end
		local dirtyServices = {}
		local restoredPendingServices = {}
		local fullSyncServices = {}
		for _, serviceName in ipairs(services) do
			if state.dirtySeqByService[serviceName] ~= nil and visible.counts[serviceName] ~= nil then
				dirtyServices[#dirtyServices + 1] = serviceName
			end
			if state.restoredPendingServices[serviceName] then
				restoredPendingServices[#restoredPendingServices + 1] = serviceName
			end
			if state.fullSyncSeqByService[serviceName] ~= nil and visible.counts[serviceName] ~= nil then
				fullSyncServices[#fullSyncServices + 1] = serviceName
			end
		end
		local propertyChanges = {}
		local propertyChangeCount = 0
		for _, change in pairs(state.propertyChangesByKey) do
			if
				requested[change.service]
				and state.dirtySeqByService[change.service] ~= nil
				and state.fullSyncSeqByService[change.service] == nil
			then
				propertyChangeCount += 1
				if not compact then
					propertyChanges[#propertyChanges + 1] = change
				end
			end
		end
		if not compact then
			table.sort(propertyChanges, function(a, b)
				return a.seq < b.seq
			end)
		end
		local changes = {}
		local changeCount = 0
		local referencePathsMayChange = #fullSyncServices > 0
		for _, change in ipairs(visible.changes) do
			if requested[change.service] and state.dirtySeqByService[change.service] ~= nil then
				changeCount += 1
				local action = string.lower(tostring(change.action or ""))
				local property = string.lower(tostring(change.property or ""))
				if action == "added" or action == "removed" or action == "fullsync" or property == "name" then
					referencePathsMayChange = true
				end
				if not compact then
					local publicChange = table.clone(change)
					publicChange.instance = nil
					publicChange.firstSeq = nil
					changes[#changes + 1] = publicChange
				end
			end
		end
		if not compact then
			table.sort(changes, function(a, b)
				return a.seq < b.seq
			end)
		end
		if changeCount == 0 and #dirtyServices > 0 then
			changeCount = #dirtyServices
			referencePathsMayChange = true
			for _, serviceName in ipairs(dirtyServices) do
				if not compact then
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
		end
		local trackedServiceCount = 0
		for _ in pairs(state.watchedServices) do
			trackedServiceCount += 1
		end
		-- Acknowledgments change pending state without changing the mutation seq.
		-- Order snapshots separately so late status replies cannot resurrect edits.
		snapshotSeq += 1
		return {
			ok = true,
			tracking = state.started,
			persistentTracking = state.persistentTracking,
			pendingEpoch = state.pendingEpoch,
			pendingRuntimeId = state.pendingRuntimeId,
			restoredPendingEpoch = state.restoredPendingEpoch,
			restoredPendingRuntimeId = state.restoredPendingRuntimeId,
			role = config.bridgeRole,
			changeTrackerVersion = CHANGE_TRACKER_VERSION,
			runtimeId = config.bridgeRuntimeId,
			seq = state.seq,
			snapshotSeq = snapshotSeq,
			dirtyServices = dirtyServices,
			restoredPendingServices = restoredPendingServices,
			fullSyncServices = fullSyncServices,
			propertyChanges = propertyChanges,
			changes = changes,
			propertyChangeCount = propertyChangeCount,
			changeCount = changeCount,
			referencePathsMayChange = referencePathsMayChange,
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

	function api.getState(params: { [string]: any }, leaseId: string?): { [string]: any }
		if params.verifyPushProof ~= nil then
			local matches, reason = api.verifyPushProof(params.verifyPushProof)
			return { ok = true, pushProofMatches = matches, pushProofMismatch = reason }
		end
		if params.start ~= false and params.stop ~= true and params.releaseTrackingGuardId == nil then
			localPushProofRequested = params.captureLocalPushProof == true
		end
		local retainedProof = nil
		if params.retainPushProof ~= nil and api.verifyPushProof(params.retainPushProof) then
			if verifiedPushProof.localObservation ~= nil then
				acquireTrackingGuard(localPushObservation.guardId, 60)
				localPushObservation.cached = true
			else
				nativeAttributeRelay.cached = true
				nativeAttributeRelay.notify:SetAttribute("CachedPushProof", true)
			end
			retainedProof = params.retainPushProof
		end
		local wasTracking = state.started
		local services = normalizeServices(params.services, allowedServices)
		if params.start ~= false and params.stop ~= true and params.releaseTrackingGuardId == nil
			and localPushObservation ~= nil and localPushObservation.cached then
			releaseLocalPushObservation()
		end
		if params.start ~= false and params.stop ~= true and params.releaseTrackingGuardId == nil
			and nativeAttributeRelay and nativeAttributeRelay.cached then
			-- A new tracking lease can follow a daemon restart before the old
			-- native worker retires. Give it a distinct relay; late old callbacks
			-- must never disarm or bypass the new observer.
			verifiedPushProof = nil
			releaseProofConnections(nativeAttributeRelay)
			releaseTagConnections()
			nativeAttributeRelay.connection:Disconnect()
			nativeAttributeRelay.frameConnection:Disconnect()
			nativeAttributeRelay.notify:Destroy()
			nativeAttributeRelay = nil
		end
		if params.deferNativeTracking == true and params.nativeAttributeRelay == true
			and params.start ~= false and params.stop ~= true and params.releaseTrackingGuardId == nil
			and not wasTracking and not state.persistentTracking
			and type(params.trackingGuardId) == "string" and params.trackingGuardId ~= ""
		then
			local path = prepareNativeAttributeRelay(services)
			if path ~= nil then
				acquireTrackingGuard(params.trackingGuardId)
				nativeAttributeRelay.pendingTracking = { guardId = params.trackingGuardId, services = services }
				return { ok = true, nativeTrackingDeferred = true, nativeAttributeRelay = path, trackingStarted = true }
			end
		end
		if params.stop == true then
			if nativeAttributeRelay then
				nativeAttributeRelay.cached = false
			end
			state.persistentTracking = false
			table.clear(state.trackingGuards)
			stopTracking()
		elseif params.releaseTrackingGuardId ~= nil then
			releaseTrackingGuard(params.releaseTrackingGuardId)
		elseif params.start ~= false then
			local trackingGuardId = params.trackingGuardId
			if type(trackingGuardId) == "string" and trackingGuardId ~= "" then
				acquireTrackingGuard(trackingGuardId)
			else
				promoteNativeAttributeConnections()
				state.persistentTracking = true
			end
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
			if nativeAttributeRelay then
				nativeAttributeRelay.pendingTracking = nil
			end
		end
		applyStateParams(params, services)

		local waitSeconds = tonumber(params.waitSeconds)
		local waitedForChange = false
		local waitTimedOut = false
		local waitCancelled = false
		if
			waitSeconds
			and waitSeconds > 0
			and params.clearPending ~= true
			and params.reset ~= true
			and params.ackSeq == nil
		then
			waitedForChange = true
			local changed
			changed, waitCancelled = waitForDirtyServices(services, waitSeconds, leaseId)
			waitTimedOut = not changed and not waitCancelled
		end

		local responseServices = if params.includeAllState == true
			then normalizeServices(nil, allowedServices)
			else services
		local response = buildStateResponse(responseServices, params.compact == true)
		response.retainedPushProof = retainedProof
		if params.capturePushProof == true then
			response.verifiedPushProof = api.capturePushProof()
		end
		if params.nativeTerrainRelay == true and state.started then
			response.nativeTerrainRelay = prepareNativeTerrainRelay()
		end
		response.trackingStarted = not wasTracking and state.started
		if params.nativeAttributeRelay == true and type(params.trackingGuardId) == "string" then
			response.nativeAttributeRelay = prepareNativeAttributeRelay(services)
		end
		if params.includeGenerations == true then
			local generations = {}
			local checkpointGenerations = {}
			for _, serviceName in ipairs(responseServices) do
				generations[serviceName] = api.serviceGeneration(serviceName)
				checkpointGenerations[serviceName] = api.checkpointGeneration(serviceName)
			end
			response.serviceGenerations = generations
			response.checkpointGenerations = checkpointGenerations
		end
		if waitedForChange then
			response.eventDriven = true
			response.waitSeconds = math.min(waitSeconds or 0, 25)
			response.waitTimedOut = waitTimedOut
			response.waitCancelled = waitCancelled
		end
		reportedSeq = state.seq
		return response
	end

	function api.cancelWait(leaseId: string?)
		if leaseId then
			for _, waiter in pairs(leasedWaits) do
				if waiter.leaseId == leaseId then
					waiter.cancel()
				end
			end
			return
		end
		state.waitGeneration += 1
		signalChange()
	end

	function api.pendingChangeCount(): number
		local count = 0
		for serviceName, retainedCount in pairs(pendingChanges().counts) do
			if state.dirtySeqByService[serviceName] ~= nil then
				count += retainedCount
			end
		end
		return count
	end

	function api.onChanged(callback): RBXScriptConnection
		return state.changeEvent.Event:Connect(callback)
	end

	function api.signalChange()
		signalChange()
	end

	function api.stop()
		verifiedPushProof = nil
		if nativeAttributeRelay then
			nativeAttributeRelay.cached = false
		end
		exportStructureObserver = nil
		exportPropertyObserver = nil
		stopTracking()
		for serviceName in pairs(serviceSignals) do
			releaseServiceSignals(serviceName)
		end
		clearExpectedEvents()
		state.changeJournal = nil
		state.changeEvent:Destroy()
	end

	return api
end

return BridgeStudioChanges
