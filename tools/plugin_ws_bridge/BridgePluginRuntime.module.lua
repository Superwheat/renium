type ServiceState = {
	instances: { Instance },
	nativeExportOnly: boolean,
	nativeSnapshotRoot: boolean?,
	nativeLiveSnapshot: boolean?,
	exportedInstances: { [Instance]: boolean }?,
	isExportedInstance: ((Instance, string) -> boolean)?,
	nonArchivableInstance: Instance?,
	nonArchivableInstances: { Instance },
	originalNonArchivableInstances: { [Instance]: boolean }?,
	nativeDebugIdBuffer: buffer?,
	nativeRootPropertyValues: { [string]: any }?,
	classNames: { string },
	classIdByName: { [string]: number },
	rootClassName: string,
	pathByInstance: { [Instance]: string },
	pathSegmentsByInstance: { [Instance]: { string } },
	pathOrdinalsByInstance: { [Instance]: { number } },
	debugIdByInstance: { [Instance]: string | boolean },
	instanceIdByInstance: { [Instance]: string | number | boolean },
	scriptObjects: { LuaSourceContainer },
	scriptIndices: { number }?,
	scriptSources: { [string]: string },
	scriptSourcesByIndex: { [number]: string },
	nativeLuaSourceIndices: { number }?,
	nativeNonArchivableIndices: { number }?,
	nativeStructureGeneration: number?,
	nativeContentGeneration: number?,
	matchedSettingsIds: { { index: number, id: string } }?,
	matchedSettingsIdVersion: number?,
	scriptInstancesByIndex: { [number]: LuaSourceContainer }?,
	scriptKeyByInstance: { [Instance]: string },
	batchCacheByKey: { [string]: string },
	batchCacheKeys: { string },
	sourceBatchCacheByKey: { [string]: string },
	sourceBatchCacheKeys: { string },
	servicePropertySchemaByClass: { [string]: { { any } } }?,
	hotPropertySchemaByClass: { [string]: { [string]: any } }?,
	nameByIndex: { [number]: string },
	classNameByIndex: { [number]: string },
	classValueByIndex: { [number]: any },
	parentIndexByIndex: { [number]: number | boolean },
	requiresPcallByClassProperty: { [string]: { [string]: boolean } },
}

local BridgePluginRuntime = {}

function BridgePluginRuntime.withSuppression(studioChanges, callback, params, scopeChanges)
	studioChanges.beginSuppress(nil, scopeChanges)
	local ok, result = pcall(callback, params)
	task.defer(studioChanges.endSuppress)
	if not ok then
		error(result, 0)
	end
	return result
end

function BridgePluginRuntime.start(context)
	-- Published places can still be streaming when plugins start. Local files
	-- are deserialized before plugins run and do not set DataModel.IsLoaded.
	if game.PlaceId > 0 and not game:IsLoaded() then
		game.Loaded:Wait()
	end
	local plugin = context.plugin
	local rootScript = context.rootScript

	local HttpService = game:GetService("HttpService")
	local EncodingService = game:GetService("EncodingService")
	local RunService = game:GetService("RunService")
	local ScriptEditorService = game:GetService("ScriptEditorService")
	local Selection = game:GetService("Selection")

	if not plugin then
		error("Renium must run as a Studio plugin")
	end

	local Config = {}
	local lifetimeConnections: { RBXScriptConnection } = {}
	local generatedRuntimeId = HttpService:GenerateGUID(false)
	if type(generatedRuntimeId) ~= "string" or generatedRuntimeId == "" then
		error("Renium could not create a bridge runtime identity")
	end
	Config.bridgeRuntimeId = generatedRuntimeId

	function Config.isPlayModeActiveForBridge(): boolean
		return not RunService:IsEdit()
	end

	function Config.getBridgeRole(): string
		if RunService:IsEdit() then
			return "edit"
		elseif RunService:IsClient() then
			return "play-client"
		end
		return "play-server"
	end

	Config.startedInPlayMode = Config.isPlayModeActiveForBridge()
	Config.bridgeRole = Config.getBridgeRole()
	Config.editorReviewUploads = {}
	local EDITOR_REVIEW_UPLOAD_TTL_SECONDS = 120
	local MAX_EDITOR_REVIEW_UPLOADS = 4
	local MAX_EDITOR_REVIEW_CHANGES = 100000
	local nextEditorReviewExpiryToken = 0

	local function pruneEditorReviewUploads()
		local now = os.clock()
		for uploadId, upload in pairs(Config.editorReviewUploads) do
			if
				type(upload) ~= "table"
				or now - (tonumber(upload.updatedAt) or 0) > EDITOR_REVIEW_UPLOAD_TTL_SECONDS
			then
				Config.editorReviewUploads[uploadId] = nil
			end
		end
	end

	local function editorReviewUploadCount(): number
		local count = 0
		for _ in pairs(Config.editorReviewUploads) do
			count += 1
		end
		return count
	end

	local function armEditorReviewUploadExpiry(uploadId: string, upload: { [any]: any })
		upload.updatedAt = os.clock()
		if upload.expiryArmed then
			return
		end
		nextEditorReviewExpiryToken += 1
		local token = nextEditorReviewExpiryToken
		upload.expiryToken = token
		upload.expiryArmed = true
		local function expireWhenIdle()
			local current = Config.editorReviewUploads[uploadId]
			if type(current) ~= "table" or current.expiryToken ~= token then
				return
			end
			local idleSeconds = os.clock() - (tonumber(current.updatedAt) or 0)
			if idleSeconds > EDITOR_REVIEW_UPLOAD_TTL_SECONDS then
				Config.editorReviewUploads[uploadId] = nil
				return
			end
			task.delay(math.max(1, EDITOR_REVIEW_UPLOAD_TTL_SECONDS - idleSeconds + 1), expireWhenIdle)
		end
		task.delay(EDITOR_REVIEW_UPLOAD_TTL_SECONDS + 1, expireWhenIdle)
	end

	function Config.getPlayerIdentity(): (string?, number?)
		if Config.bridgeRole ~= "play-client" then
			return nil, nil
		end
		local localPlayer = game:GetService("Players").LocalPlayer
		if localPlayer == nil then
			return nil, nil
		end
		return localPlayer.Name, localPlayer.UserId
	end

	local SETTINGS_PREFIX = "Renium_"
	local DEFAULT_HOST = "127.0.0.1"
	local DEFAULT_PORTS = { 8781, 8782 }
	local RECONNECT_SECONDS = 0.5
	local FAST_RECONNECT_SECONDS = 0.25
	local FAST_RECONNECT_WINDOW_SECONDS = 8.0
	local CONNECT_SESSION_TIMEOUT_SECONDS = 2.0
	local DEBUG_BRIDGE_CONNECTION = false
	local PARALLEL_SOURCE_BATCH_MIN_ITEMS = 24
	local BRIDGE_VERSION = "0.3.6"
	local BRIDGE_PROTOCOL_VERSION = "compact-v5"
	local BRIDGE_BUILD_UNIX = 1789311798
	local MAX_ACTIVE_DEMAND_SERIALIZERS = 4
	local MAX_SOURCE_BATCH_PATHS = 1024
	local COMPACT_TYPE_IDS = {
		Absent = 0,
		Bool = 1,
		Number = 2,
		String = 3,
		Vector2 = 4,
		Vector3 = 5,
		UDim = 6,
		UDim2 = 7,
		Color3 = 8,
		BrickColor = 9,
		EnumItem = 10,
		CFrame = 11,
		Rect = 12,
		Font = 13,
		ColorSequence = 14,
		NumberSequence = 15,
		Ref = 16,
		ContentId = 17,
		BinaryString = 18,
		NumberRange = 19,
		PhysicalProperties = 20,
		Axes = 21,
		Faces = 22,
		Ray = 23,
	}
	local FAST_COMPARE_EQUAL = 1
	local FAST_COMPARE_VECTOR2 = 2
	local FAST_COMPARE_VECTOR3 = 3
	local FAST_COMPARE_UDIM = 4
	local FAST_COMPARE_UDIM2 = 5
	local FAST_COMPARE_COLOR3 = 6
	local FAST_COMPARE_BRICKCOLOR = 7
	local FAST_COMPARE_ENUM_VALUE = 8
	local FAST_COMPARE_CFRAME = 9
	local FAST_COMPARE_RECT = 10
	Config.LUA_SOURCE_CLASS = {
		Script = true,
		LocalScript = true,
		ModuleScript = true,
	}
	local function requireModule(parent: Instance?, name: string): any
		if parent == nil then
			error(`Renium is missing the parent of ModuleScript {name}`)
		end
		local child = parent:FindFirstChild(name)
		if child and child:IsA("ModuleScript") then
			local result = require(child)
			if type(result) == "table" then
				return result
			end
			error(`Renium module {name} must return a table`)
		end
		error(`Renium is missing ModuleScript {name}`)
	end

	local function requireChildModule(name: string): any
		return requireModule(rootScript, name)
	end

	local SettingsModule = requireChildModule("BridgeSettings")
	local StatusModule = requireChildModule("BridgeStatus")
	local UpdateModule = requireChildModule("BridgeUpdate")
	local ParallelModule = requireChildModule("BridgeParallel")
	local ChunkingModule = requireChildModule("BridgeChunking")
	local ContentModule = requireChildModule("BridgeContent")
	local ValueEqualityModule = requireChildModule("BridgeValueEquality")
	local ValueCodecModule = requireChildModule("BridgeValueCodec")
	local CODEC_VERSION = if ValueCodecModule.configureNativeNonFiniteJson(HttpService)
		then "compact-v5-schema-9"
		else "compact-v5-schema-8"
	local TransportModule = requireChildModule("BridgeTransport")
	local ConnectionModule = requireChildModule("BridgeConnection")
	local SessionLockModule = requireChildModule("BridgeSessionLock")
	local IdentityModule = requireChildModule("BridgeIdentity")
	local MaterialServiceModule = requireChildModule("BridgeMaterialService")
	local CollisionGroupsModule = requireChildModule("BridgeCollisionGroups")
	local UiModule = requireChildModule("BridgeUi")
	local PropertySchemaModule = requireChildModule("BridgePropertySchema")
	local StudioApiSchemaModule = requireChildModule("BridgeStudioApiSchema")
	local EditorSyncModule = requireChildModule("BridgeEditorSync")
	local TransactionUploadModule = requireChildModule("BridgeTransactionUpload")
	local initialRuntimeSettings = SettingsModule.loadRuntimeSettings(plugin, SETTINGS_PREFIX)
	local activeExclusiveSessionGeneration = nil
	local editorSync
	local sessionLock
	local transactionExpectations = {}
	local RbxDomDatabase = requireModule(rootScript:FindFirstChild("RbxDom"), "database")
	sessionLock = SessionLockModule.create(
		Config.bridgeRuntimeId,
		function()
			Config.disconnectAll("Another Renium session took ownership")
		end,
		not Config.startedInPlayMode
	)

	local ui = UiModule.create(plugin, {
		version = BRIDGE_VERSION,
		buildUnix = BRIDGE_BUILD_UNIX,
		initiallyConnecting = initialRuntimeSettings.autoConnect,
	})
	ui.setPlayModeHidden(Config.isPlayModeActiveForBridge())

	Config.bridgeHost = DEFAULT_HOST
	Config.bridgePorts = DEFAULT_PORTS
	Config.bridgeChannels = {}

	local ALLOWED_SERVICES = {
		Workspace = true,
		Players = true,
		Lighting = true,
		MaterialService = true,
		ReplicatedFirst = true,
		ReplicatedStorage = true,
		ServerScriptService = true,
		ServerStorage = true,
		StarterGui = true,
		StarterPack = true,
		StarterPlayer = true,
		Teams = true,
		SoundService = true,
		VoiceChatService = true,
		TextChatService = true,
		TestService = true,
		LocalizationService = true,
		VRService = true,
	}
	Config.shouldIgnoreInstance = sessionLock.isLockInstance
	local pendingStudioChangesSettingPrefix = SETTINGS_PREFIX .. "pendingStudioChanges:"
	local function pendingStudioChangesTarget(): { [string]: any }?
		-- Plugin settings are shared by every Studio DataModel. Published places have
		-- a stable identity; local files do not expose a path to plugins, so carrying
		-- an anonymous dirty marker across processes could apply it to another file.
		if game.GameId <= 0 or game.PlaceId <= 0 then
			return nil
		end
		return {
			gameId = game.GameId,
			placeId = game.PlaceId,
			setting = pendingStudioChangesSettingPrefix .. tostring(game.GameId) .. ":" .. tostring(game.PlaceId),
		}
	end
	Config.loadPendingStudioChanges = function()
		local target = pendingStudioChangesTarget()
		if target == nil then
			return nil
		end
		local stored = plugin:GetSetting(target.setting)
		if type(stored) ~= "table"
			or (stored.version ~= 3 and stored.version ~= 4)
			or type(stored.epoch) ~= "string"
			or type(stored.services) ~= "table"
		then
			if stored ~= nil then
				plugin:SetSetting(target.setting, nil)
			end
			return nil
		end
		return {
			epoch = stored.epoch,
			runtimeId = if type(stored.runtimeId) == "string" then stored.runtimeId else nil,
			services = stored.services,
		}
	end
	Config.savePendingStudioChanges = function(services, expectedEpoch)
		local target = pendingStudioChangesTarget()
		if target == nil then
			return nil
		end
		local stored = plugin:GetSetting(target.setting)
		local storedEpoch = if type(stored) == "table" and type(stored.epoch) == "string"
			then stored.epoch
			else nil
		if storedEpoch ~= nil and storedEpoch ~= expectedEpoch then
			return nil
		end
		if #services == 0 then
			if storedEpoch ~= nil and storedEpoch == expectedEpoch then
				plugin:SetSetting(target.setting, nil)
			end
			return nil
		end
		local epoch = Config.bridgeRuntimeId .. ":" .. HttpService:GenerateGUID(false)
		plugin:SetSetting(target.setting, {
			version = 4,
			epoch = epoch,
			runtimeId = Config.bridgeRuntimeId,
			services = services,
		})
		return epoch
	end
	Config.studioChanges = requireChildModule("BridgeStudioChanges").create(Config, ALLOWED_SERVICES)
	local function beginEditorTransactionExpectation(transactionId: string, params: { [string]: any })
		table.clear(transactionExpectations)
		transactionExpectations[transactionId] = params
		Config.studioChanges.beginSuppress(nil, params)
	end
	local function finishEditorTransactionExpectation(transactionId: string)
		if transactionExpectations[transactionId] == nil then
			return
		end
		transactionExpectations[transactionId] = nil
		task.defer(Config.studioChanges.endSuppress)
	end
	local RuntimeApi = requireChildModule("BridgeRuntimeApi").create(plugin, {
		runtimeId = Config.bridgeRuntimeId,
		bridgeRole = Config.bridgeRole,
		assertSessionOwnership = function(sessionGeneration)
			if not sessionLock.validate(sessionGeneration or activeExclusiveSessionGeneration) then
				error("Renium session ownership was lost")
			end
		end,
		expectParentChange = Config.studioChanges.expectParentChange,
		expectPropertyEvent = Config.studioChanges.expectPropertyEvent,
		beginNativeRootWindow = Config.studioChanges.beginNativeRootWindow,
		endNativeRootWindow = Config.studioChanges.endNativeRootWindow,
		expectAttributeEvent = Config.studioChanges.expectAttributeEvent,
		expectTagChange = Config.studioChanges.expectTagChange,
		cancelExpectedEvent = Config.studioChanges.cancelExpectedEvent,
		studioChangeGeneration = Config.studioChanges.serviceGeneration,
		describeJournalChange = Config.studioChanges.describeJournalChange,
		assertRequestLeaseActive = function()
			editorSync.assertCurrentRequestLeaseActive()
		end,
	})
	Config.creatorApi = requireChildModule("BridgeCreatorApi").create({
		assertRequestLeaseActive = function()
			editorSync.assertCurrentRequestLeaseActive()
		end,
	})
	local networkSimulation = requireChildModule("BridgeNetworkSimulation").create(function()
		return settings():GetService("NetworkSettings")
	end)
	-- Roblox's version() global is available in Studio but absent from Selene's stdlib.
	-- selene: allow(undefined_variable)
	local studioVersion = version()
	local performance = requireChildModule("BridgePerformance").create({
		stats = game:GetService("Stats"), runService = RunService,
		studioVersion = studioVersion, bridgeVersion = BRIDGE_VERSION,
		timestamp = function() return DateTime.now():ToIsoDate() end,
		memoryTags = Enum.DeveloperMemoryTag:GetEnumItems(),
		microProfiler = function() return game:GetService("MicroProfilerService") end,
		encodeBuffer = function(data) return buffer.tostring(EncodingService:Base64Encode(data)) end,
		runtimeId = Config.bridgeRuntimeId, client = Config.bridgeRole == "play-client",
		edit = Config.bridgeRole == "edit",
		newId = function() return HttpService:GenerateGUID(false) end,
		delay = task.delay, cancel = task.cancel,
	})
	local microProfiler = requireChildModule("BridgeMicroProfiler").create(function()
		return game:GetService("MicroProfilerService")
	end, task.wait)
	function Config.applyBridgeRuntimeSettings(runtimeSettings: { [string]: any })
		Config.studioChanges.setOptions({
			syncbackProperties = runtimeSettings.syncbackProperties,
			onlyCodeMode = runtimeSettings.onlyCodeMode,
		})
	end
	do
		local storedConflictResolution = SettingsModule.loadConflictResolution(plugin, SETTINGS_PREFIX, nil)
		if storedConflictResolution then
			Config.studioChanges.setConflictResolution(storedConflictResolution)
		end
	end

	local PROPERTY_CANDIDATES = {
		"Archivable",
		"Enabled",
		"RunContext",
		"Disabled",
		"LinkedSource",
		"Value",
		"Name",
		"ClassName",
		"Parent",
		"Part0",
		"Part1",
		"Attachment0",
		"Attachment1",
		"AutoLocalize",
		"RootLocalizationTable",
		"BackgroundColor3",
		"BackgroundTransparency",
		"BorderColor3",
		"BorderSizePixel",
		"Position",
		"Size",
		"AnchorPoint",
		"Rotation",
		"Visible",
		"Text",
		"TextColor3",
		"TextSize",
		"TextScaled",
		"FontFace",
		"Image",
		"ImageColor3",
		"ImageTransparency",
		"Color",
		"Transparency",
		"ZIndex",
		"LayoutOrder",
		"Active",
		"Selectable",
		"CanvasSize",
		"ScrollBarThickness",
		"AutomaticCanvasSize",
		"RichText",
		"LineHeight",
		"MaxVisibleGraphemes",
		"SliceCenter",
		"ScaleType",
		"TileSize",
		"Padding",
		"CellPadding",
		"CellSize",
		"FillDirection",
		"SortOrder",
		"HorizontalAlignment",
		"VerticalAlignment",
		"ApplyStrokeMode",
		"Thickness",
		"Color3",
		"Material",
		"BrickColor",
		"CanCollide",
		"CanQuery",
		"CanTouch",
		"Massless",
		"Anchored",
		"CastShadow",
		"CFrame",
		"Orientation",
		"AssemblyLinearVelocity",
		"AssemblyAngularVelocity",
		"Shape",
		"Reflectance",
		"TopSurface",
		"BottomSurface",
		"LeftSurface",
		"RightSurface",
		"FrontSurface",
		"BackSurface",
		"LightInfluence",
		"Brightness",
		"ClockTime",
		"FogColor",
		"FogEnd",
		"FogStart",
		"GeographicLatitude",
		"GlobalShadows",
		"EnvironmentDiffuseScale",
		"EnvironmentSpecularScale",
		"Ambient",
		"OutdoorAmbient",
		"Technology",
	}

	local NO_DEFAULTS = {}
	local NO_PROPERTIES = {}
	local DEFAULT_PROPERTY_CACHE: { [string]: any } = {}
	local DEFAULT_TRANSPORT_PROPERTY_CACHE: { [string]: any } = {}
	local DEFAULT_TRANSPORT_FAST_COMPARE_CACHE: { [string]: any } = {}
	local ENUM_VALUE_NAMES_BY_TYPE_CACHE: { [string]: any } = {}
	local CLASS_PROPERTY_CANDIDATES_CACHE: { [string]: any } = {}
	local CLASS_PROPERTY_SCHEMA_CACHE: { [string]: any } = {}
	local configuredExportAllProperties = plugin:GetSetting(SETTINGS_PREFIX .. "exportAllProperties")
	local EXPORT_ALL_PROPERTIES = configuredExportAllProperties == true
	local BUNDLED_PROPERTY_SCHEMAS_BY_CLASS: { [string]: { { any } } } =
		PropertySchemaModule.buildSchemasFromRbxDom(RbxDomDatabase, COMPACT_TYPE_IDS, StudioApiSchemaModule)
	local EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS: { [string]: { { any } } } = BUNDLED_PROPERTY_SCHEMAS_BY_CLASS
	local EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS: { [string]: { string } } =
		PropertySchemaModule.buildCandidatesFromSchemas(EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS)

	Config.studioChanges.configurePropertyCandidates(EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS)

	local stateByService: { [string]: ServiceState }
	local nativeStateByService: { [string]: ServiceState } = {}
	local demandSerializerGate = Instance.new("BindableEvent")
	local activeDemandSerializers = 0
	stateByService = {}
	function Config.invalidateRuntimeExportCache(serviceName: string)
		stateByService[serviceName] = nil
		nativeStateByService[serviceName] = nil
	end
	local editorActions = {}
	local editorActionCounter = 0
	local function queueEditorAction(action: { [string]: any })
		editorActionCounter += 1
		action.id = tostring(editorActionCounter)
		editorActions[#editorActions + 1] = action
	end

	local function selectedScriptAction()
		local selected = Selection:Get()
		local selectedScript = nil
		for _, instance in ipairs(selected) do
			if instance:IsA("LuaSourceContainer") then
				selectedScript = instance
				break
			end
		end
		if selectedScript == nil then
			ui.notify(
				"reveal-script",
				"No script is selected",
				"Select a script in Studio, then run Reveal Script in Editor again.",
				nil,
				nil,
				false
			)
			ui.showWidget()
			return
		end
		local pathSegments, pathOrdinals = IdentityModule.getRefPathParts(selectedScript)
		local serviceName = if pathSegments then tostring(pathSegments[1] or "") else ""
		if pathSegments == nil or pathOrdinals == nil or not ALLOWED_SERVICES[serviceName] then
			ui.notify(
				"reveal-script",
				"Selected script is outside the synced tree",
				"Move it under a service Renium syncs, then try again.",
				nil,
				nil,
				false
			)
			ui.showWidget()
			return
		end
		local settingsId = nil
		local state = stateByService[serviceName]
		if state ~= nil then
			settingsId = IdentityModule.getCachedInstanceId(state, selectedScript)
		end
		queueEditorAction({
			type = "revealScript",
			service = serviceName,
			settingsId = settingsId,
			pathSegments = pathSegments,
			pathOrdinals = pathOrdinals,
		})
	end

	local function pendingEditorActions(acknowledged: any, runtimeId: any)
		if type(acknowledged) == "table" and #acknowledged > 0 then
			if type(runtimeId) ~= "string" or runtimeId ~= Config.bridgeRuntimeId then
				error("Editor action acknowledgment runtime does not match")
			end
			local acknowledgedIds = {}
			for _, id in ipairs(acknowledged) do
				acknowledgedIds[tostring(id)] = true
			end
			local kept = {}
			for _, action in ipairs(editorActions) do
				if not acknowledgedIds[action.id] then
					kept[#kept + 1] = action
				end
			end
			editorActions = kept
		end
		return table.clone(editorActions)
	end

	lifetimeConnections[#lifetimeConnections + 1] = ui.actions.reveal.Triggered:Connect(selectedScriptAction)

	-- Studio selects every object that undoing or redoing a waypoint restores.
	-- A Renium sync can restore hundreds, so the selection the user had is put
	-- back after Studio reacts to one of Renium's own waypoints.
	do
		local ChangeHistoryService = game:GetService("ChangeHistoryService")
		local previousSelection: { Instance } = Selection:Get()
		local currentSelection: { Instance } = previousSelection
		local selectionChangedAt = -math.huge
		local keepSelection: { Instance }? = nil
		local keepUntil = 0
		local restoring = false
		local function sameSelection(left: { Instance }, right: { Instance }): boolean
			if #left ~= #right then
				return false
			end
			for index, instance in ipairs(left) do
				if right[index] ~= instance then
					return false
				end
			end
			return true
		end
		local function restoreSelection(selection: { Instance })
			local kept = {}
			for _, instance in ipairs(selection) do
				if instance:IsDescendantOf(game) then
					kept[#kept + 1] = instance
				end
			end
			restoring = true
			Selection:Set(kept)
			restoring = false
			currentSelection = kept
		end
		lifetimeConnections[#lifetimeConnections + 1] = Selection.SelectionChanged:Connect(function()
			if restoring then
				return
			end
			previousSelection = currentSelection
			currentSelection = Selection:Get()
			selectionChangedAt = os.clock()
			local keep = keepSelection
			if keep ~= nil and os.clock() < keepUntil and not sameSelection(currentSelection, keep) then
				keepSelection = nil
				restoreSelection(keep)
			end
		end)
		local function onReniumWaypoint(name: string)
			if type(name) ~= "string" or string.sub(name, 1, 7) ~= "Renium:" then
				return
			end
			local keep = if os.clock() - selectionChangedAt < 0.25 then previousSelection else currentSelection
			keepSelection = keep
			keepUntil = os.clock() + 1
			if not sameSelection(currentSelection, keep) then
				keepSelection = nil
				restoreSelection(keep)
			end
		end
		lifetimeConnections[#lifetimeConnections + 1] = ChangeHistoryService.OnUndo:Connect(onReniumWaypoint)
		lifetimeConnections[#lifetimeConnections + 1] = ChangeHistoryService.OnRedo:Connect(onReniumWaypoint)
	end

	Config.bridgeConnectRequested = false
	Config.bridgeConnectedOnce = false
	Config.bridgeConnectSession = 0
	Config.bridgeConnectDeadline = 0
	Config.bridgeConnectionStatus = "Disconnected"

	local editorSyncStats = {
		requests = 0,
		lastMs = 0,
		sourceCreated = 0,
		sourceUpdated = 0,
		sourceDeleted = 0,
		instanceCreated = 0,
		instanceReplaced = 0,
		instanceDeleted = 0,
		propertyUpdated = 0,
		attributeUpdated = 0,
		noops = 0,
		errors = 0,
		lastAtUnix = 0,
		lastOk = true,
	}
	local getClassPropertySchema
	local encodeSchemaComparableValue
	local propertyKey
	local serializeAttributesCompactV5
	local prepareService
	local getState

	local function excludedExportRoots(serviceName: string, service: Instance): { Instance }
		local roots = {}
		if serviceName == "ServerStorage" then
			for _, child in ipairs(service:GetChildren()) do
				if sessionLock.isLockInstance(child) then
					roots[#roots + 1] = child
				end
			end
		elseif serviceName == "Players" then
			for _, child in ipairs(service:GetChildren()) do
				if child:IsA("Player") then
					roots[#roots + 1] = child
				end
			end
		end
		return roots
	end

	local function includeExportInstance(serviceName: string, instance: Instance): boolean
		if instance:IsA("TouchTransmitter") then
			return false
		end
		if serviceName == "ServerStorage" then
			return not sessionLock.isLockInstance(instance)
		end
		return serviceName ~= "Players"
			or not instance:IsA("Player") and not instance:FindFirstAncestorWhichIsA("Player")
	end
	Config.includeExportInstance = includeExportInstance
	local nativeStructureGenerationByService: { [string]: number } = {}
	local nativeContentGenerationByService: { [string]: number } = {}
	local nativeArchivableGenerationByService: { [string]: number } = {}
	local nativeLastChangeByService: { [string]: string } = {}
	local nativeServiceNames: { [Instance]: string } = {}
	local exportPropertyRelevant = PropertySchemaModule.makeExportPropertyFilter(RbxDomDatabase, EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS)
	for serviceName in pairs(ALLOWED_SERVICES) do
		nativeStructureGenerationByService[serviceName] = 0
		nativeContentGenerationByService[serviceName] = 0
		local service = game:GetService(serviceName)
		nativeServiceNames[service] = serviceName
	end
	Config.exportProofGeneration = function(serviceName: string): (number, string?)
		return nativeContentGenerationByService[serviceName] + (nativeArchivableGenerationByService[serviceName] or 0), nativeLastChangeByService[serviceName]
	end
	Config.invalidateTerrainExport = function()
		nativeContentGenerationByService.Workspace += 1
		nativeLastChangeByService.Workspace = "SmoothGrid"
	end
	Config.captureTerrainProof = function(): string
		local payload = game:GetService("SerializationService"):SerializeInstancesAsync({ game:GetService("Workspace").Terrain })
		return buffer.tostring(EncodingService:ComputeBufferHash(payload, Enum.HashAlgorithm.Blake3))
	end
	Config.observeProofDocuments = function(changed): { RBXScriptConnection }
		local editor = game:GetService("ScriptEditorService")
		return { editor.TextDocumentDidChange:Connect(changed), editor.TextDocumentDidOpen:Connect(changed),
			editor.TextDocumentDidClose:Connect(changed) }
	end
	local function markStructureChanged(serviceName: string, instance: Instance)
		local included = includeExportInstance(serviceName, instance)
		if included then
			nativeStructureGenerationByService[serviceName] += 1
			nativeContentGenerationByService[serviceName] += 1
			nativeLastChangeByService[serviceName] = "hierarchy"
		end
		return included
	end
	-- Export includes camera objects even though Live Sync deliberately ignores
	-- their edits. Its bytes therefore need a separate invalidation generation.
	Config.studioChanges.observeExports(markStructureChanged, function(instance, propertyName)
		-- Archivable is restored from the overlay; serialization temporarily sets it.
		if typeof(instance) == "Instance" and exportPropertyRelevant(instance.ClassName, tostring(propertyName)) then
			local root = Config.studioChanges.trackedServiceRoot(instance)
			if root == nil then
				root = instance
				while root.Parent ~= nil and root.Parent ~= game do
					root = root.Parent
				end
			end
			local serviceName = nativeServiceNames[root]
			if serviceName ~= nil and includeExportInstance(serviceName, instance) then
				nativeLastChangeByService[serviceName] = tostring(propertyName)
				if string.lower(tostring(propertyName)) == "archivable" then
					nativeArchivableGenerationByService[serviceName] = (nativeArchivableGenerationByService[serviceName] or 0) + 1
				else
					nativeContentGenerationByService[serviceName] += 1
					if propertyName == "Parent" then
						-- Reparenting within a service changes serializer order without
						-- firing that service's DescendantAdded/Removing signals.
						nativeStructureGenerationByService[serviceName] += 1
						Config.studioChanges.invalidateExportCache(serviceName)
					end
				end
			end
		end
	end)

	function Config.updateStatusText()
		local statusState = {
			bridgeVersion = BRIDGE_VERSION,
			bridgeBuildUnix = BRIDGE_BUILD_UNIX,
			codecVersion = CODEC_VERSION,
			host = Config.bridgeHost,
			ports = Config.bridgePorts,
			connectionStatus = Config.bridgeConnectionStatus,
			connectRequested = Config.bridgeConnectRequested,
			channels = Config.bridgeChannels,
			editorSyncStats = editorSyncStats,
			runtimeId = Config.bridgeRuntimeId,
			target = if game.PlaceId > 0 then `{game.Name} ({game.PlaceId})` else game.Name,
			pendingReviewCount = ui.pendingReviewCount(),
			pendingEditCount = Config.studioChanges.pendingChangeCount(),
		}
		ui.updateStatus(StatusModule.view(statusState))
	end

	function Config.recordSyncCompletion()
		editorSyncStats.lastAtUnix = os.time()
		editorSyncStats.lastOk = true
		Config.updateStatusText()
	end

	local refreshMatchedSettingsIds
	local matchedSettingsIdsForRange
	local function nativeExportGeneration(serviceName: string): number
		-- ItemChanged covers ignored camera edits, but does not cover every
		-- attribute event. The tracker covers those without another listener set.
		return nativeContentGenerationByService[serviceName] + Config.studioChanges.serviceGeneration(serviceName)
			+ Config.studioChanges.tagGeneration()
	end
	editorSync = EditorSyncModule.create({
		stats = editorSyncStats,
		serializeValue = function(value)
			return Config.serializeEditorValue(value, nil)
		end,
		runtimeId = Config.bridgeRuntimeId,
		allowedServices = ALLOWED_SERVICES,
		maxChangesPerRequest = 5000,
		maxInstanceEntriesPerChange = 5000,
		maxPathSegments = 128,
		maxSourceBytes = 8 * 1024 * 1024,
		luaSourceClass = Config.LUA_SOURCE_CLASS,
		identityModule = IdentityModule,
		getState = function(serviceName: string)
			return getState(serviceName)
		end,
		prepareNativeState = function(serviceName: string, scriptSourcesByInstance)
			local structureGeneration = nativeStructureGenerationByService[serviceName] or 0
			local contentGeneration = nativeExportGeneration(serviceName)
			local cached = nativeStateByService[serviceName]
			if cached ~= nil and cached.nativeStructureGeneration == structureGeneration then
				if not Config.studioChanges.isTracking(serviceName) or cached.nativeContentGeneration ~= contentGeneration then
					table.clear(cached.batchCacheByKey)
					table.clear(cached.batchCacheKeys)
				end
				table.clear(cached.sourceBatchCacheByKey)
				table.clear(cached.sourceBatchCacheKeys)
				table.clear(cached.scriptObjects)
				table.clear(cached.scriptSourcesByIndex)
				for _, instanceIndex in ipairs(cached.nativeLuaSourceIndices or {}) do
					local instance = cached.instances[instanceIndex]
					local source = if scriptSourcesByInstance then scriptSourcesByInstance[instance] else nil
					if source ~= nil then
						cached.scriptObjects[#cached.scriptObjects + 1] = instance
						cached.scriptSourcesByIndex[instanceIndex] = source
					end
				end
				local nonArchivableInstances = cached.nonArchivableInstances or {}
				local nonArchivableIndices = cached.nativeNonArchivableIndices or {}
				table.clear(nonArchivableInstances)
				table.clear(nonArchivableIndices)
				local nonArchivableInstance = nil
				if Config.studioChanges.hasNonArchivable(serviceName) ~= false then
					for instanceIndex, instance in ipairs(cached.instances) do
						if not instance.Archivable then
							nonArchivableInstance = nonArchivableInstance or instance
							nonArchivableInstances[#nonArchivableInstances + 1] = instance
							nonArchivableIndices[#nonArchivableIndices + 1] = instanceIndex
						end
					end
				end
				cached.nonArchivableInstance = nonArchivableInstance
				cached.nonArchivableInstances = nonArchivableInstances
				cached.nativeNonArchivableIndices = nonArchivableIndices
				if refreshMatchedSettingsIds(cached) then
					table.clear(cached.batchCacheByKey)
					table.clear(cached.batchCacheKeys)
				end
				cached.nativeContentGeneration = contentGeneration
				return cached
			end
			local trackedInstances = Config.studioChanges.exportInstances(serviceName)
			local _, state = prepareService(serviceName, true, nil, scriptSourcesByInstance, trackedInstances)
			state.nativeStructureGeneration = structureGeneration
			state.nativeContentGeneration = contentGeneration
			nativeStateByService[serviceName] = state
			return state
		end,
		includeExportInstance = includeExportInstance,
		nativeExportGeneration = nativeExportGeneration,
		capturePushProof = Config.studioChanges.capturePushProof,
		getPropertySchema = function(className: string)
			return getClassPropertySchema(className) or {}
		end,
		getEnumValueNames = function(enumType: string)
			return Config.getEditorBinaryEnumValueNames(enumType)
		end,
		invalidateService = function(serviceName: string)
			Config.studioChanges.invalidateExportCache(serviceName)
			Config.invalidateRuntimeExportCache(serviceName)
		end,
		updateStatus = Config.updateStatusText,
		getSyncOptions = function()
			return Config.getBridgeSettings()
		end,
		readRootProperties = function(serviceName: string, state: ServiceState)
			return Config.readEditorBinaryRootProperties(serviceName, state)
		end,
		captureRootProperties = function(serviceName: string)
			return Config.captureEditorBinaryRootProperties(serviceName)
		end,
		assertSessionOwnership = function()
			if not sessionLock.validate(activeExclusiveSessionGeneration) then
				error("Renium session ownership was lost")
			end
		end,
		expectParentChange = Config.studioChanges.expectParentChange,
		expectPropertyEvent = Config.studioChanges.expectPropertyEvent,
		samplePropertyChange = Config.studioChanges.samplePropertyChange,
		expectAttributeEvent = Config.studioChanges.expectAttributeEvent,
		expectTagChange = Config.studioChanges.expectTagChange,
		cancelExpectedEvent = Config.studioChanges.cancelExpectedEvent,
		beginStudioChangeSuppression = Config.studioChanges.beginSuppress,
		endStudioChangeSuppression = Config.studioChanges.endSuppress,
		beginStudioChangeJournal = Config.studioChanges.beginChangeJournal,
		drainStudioChangeJournal = Config.studioChanges.drainChangeJournal,
		finishStudioChangeJournal = Config.studioChanges.finishChangeJournal,
		beginNativeImportObservations = Config.studioChanges.beginNativeImportObservations,
		finishNativeImportObservations = Config.studioChanges.finishNativeImportObservations,
		finishEditorTransactionExpectation = finishEditorTransactionExpectation,
		studioChangeGeneration = Config.studioChanges.serviceGeneration,
		describeJournalChange = Config.studioChanges.describeJournalChange,
		isStudioChangeTracking = Config.studioChanges.isTracking,
		hasNonArchivable = Config.studioChanges.hasNonArchivable,
		trackedExportInstances = Config.studioChanges.exportInstances,
		attributeObservation = Config.studioChanges.attributeObservation,
	})

	refreshMatchedSettingsIds = function(state: ServiceState)
		local version = editorSync.matchedSettingsIdVersion()
		if state.matchedSettingsIdVersion == version then
			return false
		end
		local rows = {}
		if version > 0 then
			for index, instance in ipairs(state.instances) do
				local settingsId = editorSync.matchedSettingsId(instance)
				if settingsId ~= nil then
					rows[#rows + 1] = { index = index, id = settingsId }
				end
			end
		end
		state.matchedSettingsIds = rows
		state.matchedSettingsIdVersion = version
		return true
	end

	matchedSettingsIdsForRange = function(
		state: ServiceState,
		startIndex: number,
		count: number
	): { { any } }?
		refreshMatchedSettingsIds(state)
		local finish = startIndex + count - 1
		local rows = {}
		local matched = state.matchedSettingsIds or {}
		local low = 1
		local high = #matched + 1
		while low < high do
			local middle = math.floor((low + high) / 2)
			if matched[middle].index < startIndex then
				low = middle + 1
			else
				high = middle
			end
		end
		for index = low, #matched do
			local row = matched[index]
			if row.index > finish then
				break
			end
			if row.index >= startIndex and row.index <= finish then
				rows[#rows + 1] = { row.index - startIndex + 1, row.id }
			end
		end
		return if #rows > 0 then rows else nil
	end
	local function tryReadModelPivotProperty(instance: Instance, propertyName: string): (boolean, any)
		if not (instance:IsA("Model") or instance:IsA("WorldModel")) then
			return false, nil
		end
		if propertyName == "Scale" then
			return true, (instance :: any):GetScale()
		elseif propertyName == "WorldPivotData" or propertyName == "WorldPivot" then
			return true, (instance :: any).WorldPivot
		elseif propertyName == "Origin" then
			return true, (instance :: any):GetPivot()
		end
		return false, nil
	end

	local function tryRead(instance: Instance, propertyName: string): (boolean, any)
		local okModelPivot, modelPivotValue = tryReadModelPivotProperty(instance, propertyName)
		if okModelPivot then
			return true, modelPivotValue
		end
		local isMaterialOverride, materialOverride = MaterialServiceModule.readOverride(instance, propertyName)
		if isMaterialOverride then
			return true, materialOverride
		end
		return pcall(function()
			return (instance :: any)[propertyName]
		end)
	end

	local function physicalPropertiesComparable(value: any): { number }?
		if typeof(value) ~= "PhysicalProperties" then
			return nil
		end
		return {
			(value :: any).Density,
			(value :: any).Friction,
			(value :: any).Elasticity,
			(value :: any).FrictionWeight,
			(value :: any).ElasticityWeight,
			(value :: any).AcousticAbsorption,
		}
	end

	local function physicalPropertiesObject(value: any): any?
		local comparable = physicalPropertiesComparable(value)
		if comparable == nil then
			return nil
		end
		local encoded = ValueCodecModule.encodeComponents(table.unpack(comparable, 1, 6))
		return {
			_type = "PhysicalProperties",
			customPhysics = true,
			density = encoded[1],
			friction = encoded[2],
			elasticity = encoded[3],
			frictionWeight = encoded[4],
			elasticityWeight = encoded[5],
			acousticAbsorption = encoded[6],
		}
	end

	local function normalizeSchemaTransportValue(
		typeId: number,
		propertyName: string,
		instance: Instance,
		hasValue: boolean,
		value: any
	): (boolean, any)
		if
			hasValue
			and value == nil
			and typeId == COMPACT_TYPE_IDS.PhysicalProperties
			and propertyName == "CustomPhysicalProperties"
		then
			if instance:IsA("BasePart") then
				return true, false
			end
		end
		return hasValue, value
	end

	function Config.getClassPropertyFallbackMap(state: ServiceState, className: string): { [string]: boolean }
		local fallbackByClass = state.requiresPcallByClassProperty
		local fallbackMap = fallbackByClass[className]
		if fallbackMap == nil then
			fallbackMap = {}
			fallbackByClass[className] = fallbackMap
		end
		return fallbackMap
	end

	local function serializeValue(value: any, state: ServiceState?): any
		local valueType = typeof(value)
		if valueType == "string" or valueType == "boolean" then
			return value
		elseif valueType == "number" then
			return ValueCodecModule.encodeNumber(value)
		elseif valueType == "Vector2" then
			local components = ValueCodecModule.encodeComponents(value.X, value.Y)
			return { _type = "Vector2", x = components[1], y = components[2] }
		elseif valueType == "Vector3" then
			local components = ValueCodecModule.encodeComponents(value.X, value.Y, value.Z)
			return { _type = "Vector3", x = components[1], y = components[2], z = components[3] }
		elseif valueType == "UDim" then
			local components = ValueCodecModule.encodeComponents(value.Scale, value.Offset)
			return { _type = "UDim", scale = components[1], offset = components[2] }
		elseif valueType == "UDim2" then
			local components =
				ValueCodecModule.encodeComponents(value.X.Scale, value.X.Offset, value.Y.Scale, value.Y.Offset)
			return {
				_type = "UDim2",
				xScale = components[1],
				xOffset = components[2],
				yScale = components[3],
				yOffset = components[4],
			}
		elseif valueType == "Color3" then
			local components = ValueCodecModule.encodeComponents(value.R, value.G, value.B)
			return { _type = "Color3", r = components[1], g = components[2], b = components[3] }
		elseif valueType == "BrickColor" then
			return { _type = "BrickColor", number = value.Number }
		elseif valueType == "NumberRange" then
			local components = ValueCodecModule.encodeComponents(value.Min, value.Max)
			return { _type = "NumberRange", min = components[1], max = components[2] }
		elseif valueType == "PhysicalProperties" then
			return physicalPropertiesObject(value)
		elseif valueType == "ColorSequence" then
			local keypoints = {}
			for i, keypoint in ipairs(value.Keypoints) do
				local components = ValueCodecModule.encodeComponents(
					keypoint.Time,
					keypoint.Value.R,
					keypoint.Value.G,
					keypoint.Value.B
				)
				keypoints[i] = {
					time = components[1],
					value = { r = components[2], g = components[3], b = components[4] },
				}
			end
			return { _type = "ColorSequence", keypoints = keypoints }
		elseif valueType == "NumberSequence" then
			local keypoints = {}
			for i, keypoint in ipairs(value.Keypoints) do
				local components = ValueCodecModule.encodeComponents(keypoint.Time, keypoint.Value, keypoint.Envelope)
				keypoints[i] = { time = components[1], value = components[2], envelope = components[3] }
			end
			return { _type = "NumberSequence", keypoints = keypoints }
		elseif valueType == "CFrame" then
			return { _type = "CFrame", components = ValueCodecModule.encodeComponents(value:GetComponents()) }
		elseif valueType == "Rect" then
			local components = ValueCodecModule.encodeComponents(value.Min.X, value.Min.Y, value.Max.X, value.Max.Y)
			return {
				_type = "Rect",
				minX = components[1],
				minY = components[2],
				maxX = components[3],
				maxY = components[4],
			}
		elseif valueType == "EnumItem" then
			return { _type = "EnumItem", enumType = tostring(value.EnumType), name = value.Name }
		elseif valueType == "Font" then
			return {
				_type = "Font",
				family = value.Family,
				weight = tostring(value.Weight),
				style = tostring(value.Style),
			}
		elseif valueType == "Axes" then
			local axes = {}
			if value.X then
				axes[#axes + 1] = "X"
			end
			if value.Y then
				axes[#axes + 1] = "Y"
			end
			if value.Z then
				axes[#axes + 1] = "Z"
			end
			return { _type = "Axes", axes = axes }
		elseif valueType == "Faces" then
			local faces = {}
			if value.Right then
				faces[#faces + 1] = "Right"
			end
			if value.Top then
				faces[#faces + 1] = "Top"
			end
			if value.Back then
				faces[#faces + 1] = "Back"
			end
			if value.Left then
				faces[#faces + 1] = "Left"
			end
			if value.Bottom then
				faces[#faces + 1] = "Bottom"
			end
			if value.Front then
				faces[#faces + 1] = "Front"
			end
			return { _type = "Faces", faces = faces }
		elseif valueType == "Ray" then
			local components = ValueCodecModule.encodeComponents(
				value.Origin.X,
				value.Origin.Y,
				value.Origin.Z,
				value.Direction.X,
				value.Direction.Y,
				value.Direction.Z
			)
			return {
				_type = "Ray",
				origin = { x = components[1], y = components[2], z = components[3] },
				direction = { x = components[4], y = components[5], z = components[6] },
			}
		elseif valueType == "Content" then
			return ContentModule.serialize(value)
		elseif valueType == "Instance" then
			return IdentityModule.serializeRefValue(state, value)
		end
		return nil
	end

	local function serializeContentValue(value: any): string?
		if type(value) == "string" then
			return value
		end
		if typeof(value) == "Content" then
			return ContentModule.serialize(value)
		end
		return nil
	end

	local function axesBitmask(value: any): number
		return (if value.X then 1 else 0) + (if value.Y then 2 else 0) + (if value.Z then 4 else 0)
	end

	local function facesBitmask(value: any): number
		return (if value.Right then 1 else 0)
			+ (if value.Top then 2 else 0)
			+ (if value.Back then 4 else 0)
			+ (if value.Left then 8 else 0)
			+ (if value.Bottom then 16 else 0)
			+ (if value.Front then 32 else 0)
	end

	local deepEqual = ValueEqualityModule.exactValuesEqual

	Config.serializeEditorValue = serializeValue

	local function normalizePropertyName(name: string): string
		return string.match(name, "^%s*(.-)%s*$")
	end

	propertyKey = function(name: string): string
		return string.lower(name)
	end

	local TRANSIENT_TRANSPORT_PROPERTIES: { [string]: boolean } = {
		absoluteposition = true,
		absoluterotation = true,
		absolutesize = true,
		absolutecanvassize = true,
		absolutewindowsize = true,
		absolutecontentsize = true,
		absolutecellcount = true,
		absolutecellsize = true,
		absolutepositionwrite = true,
		absolutesizewrite = true,
		arehingesdetected = true,
		channelcount = true,
		datamodelplaceversion = true,
		floormaterial = true,
		ispaused = true,
		issmooth = true,
		isspatial = true,
		lastusedmodificationmethod = true,
		localizedtext = true,
		localizationmatchedsourcetext = true,
		localizationmatchidentifier = true,
		maxextents = true,
		movedirection = true,
		movedirectioninternal = true,
		occupant = true,
		opentypefeatureserror = true,
		physicsreprrootpart = true,
		rolloffgain = true,
		rootpart = true,
		seatpart = true,
		steer = true,
		terrain = true,
		throttle = true,
		timeposition = true,
		timepositionreplicating = true,
		timepositionreplicator = true,
		resolution = true,
		walkdirection = true,
		weightcurrent = true,
		weighttarget = true,
		contenttext = true,
		textbounds = true,
		textfits = true,
		assemblyangularvelocity = true,
		assemblylinearvelocity = true,
		assemblycenterofmass = true,
		assemblymass = true,
		assemblyrootpart = true,
		centerofmass = true,
		currentcamera = true,
		currentphysicalproperties = true,
		distributedgametime = true,
		extentscframe = true,
		extentssize = true,
		isloaded = true,
		isplaying = true,
		mass = true,
		networkissleeping = true,
		playbackloudness = true,
		receiveage = true,
		rotvelocity = true,
		timelength = true,
		velocity = true,
	}

	local function shouldSkipStructuralTransportProperty(className: string, propertyName: string): boolean
		local key = propertyKey(propertyName)
		return TRANSIENT_TRANSPORT_PROPERTIES[key]
			or key == "source"
			or key == "robloxlocked"
			or key == "name"
			or key == "classname"
			or key == "parent"
			or (key == "runcontext" and className ~= "Script")
	end

	local function getDefaultSerializedProperties(className: string): any
		local cached = DEFAULT_PROPERTY_CACHE[className]
		if cached ~= nil then
			if cached == NO_DEFAULTS then
				return nil
			end
			return cached
		end

		local ok, probe = pcall(Instance.new, className)
		if not ok or probe == nil then
			DEFAULT_PROPERTY_CACHE[className] = NO_DEFAULTS
			return nil
		end

		local defaults = {}
		local classCandidates = {}
		local candidateSource = EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS[className] or PROPERTY_CANDIDATES
		for _, propertyName in ipairs(candidateSource) do
			if not shouldSkipStructuralTransportProperty(className, propertyName) then
				local got, value = tryRead(probe, propertyName)
				if got then
					table.insert(classCandidates, propertyName)
					if value ~= nil then
						local serialized = serializeValue(value, nil)
						if serialized ~= nil then
							defaults[propertyName] = serialized
						end
					end
				end
			end
		end
		probe:Destroy()

		if #classCandidates > 0 then
			CLASS_PROPERTY_CANDIDATES_CACHE[className] = classCandidates
		else
			CLASS_PROPERTY_CANDIDATES_CACHE[className] = NO_PROPERTIES
		end
		DEFAULT_PROPERTY_CACHE[className] = defaults
		return defaults
	end

	local function getDefaultTransportProperties(className: string): any
		local cached = DEFAULT_TRANSPORT_PROPERTY_CACHE[className]
		if cached ~= nil then
			if cached == NO_DEFAULTS then
				return nil
			end
			return cached
		end

		local ok, probe = pcall(Instance.new, className)
		if not ok or probe == nil then
			DEFAULT_TRANSPORT_PROPERTY_CACHE[className] = NO_DEFAULTS
			DEFAULT_TRANSPORT_FAST_COMPARE_CACHE[className] = NO_DEFAULTS
			return nil
		end

		local defaults = {}
		local fastDefaults = {}
		local propertySchema = getClassPropertySchema(className) or {}
		for _, schemaEntry in ipairs(propertySchema) do
			local propertyName = schemaEntry[1]
			local typeId = schemaEntry[2]
			local enumType = if type(schemaEntry[3]) == "string" then schemaEntry[3] else nil
			local got, value = tryRead(probe, propertyName)
			if got and value ~= nil then
				local comparable = encodeSchemaComparableValue(typeId, enumType, value, nil)
				if comparable ~= nil then
					defaults[propertyName] = comparable
					if typeId == COMPACT_TYPE_IDS.EnumItem and typeof(value) == "EnumItem" then
						fastDefaults[propertyName] = value.Value
					else
						fastDefaults[propertyName] = comparable
					end
				end
			end
		end
		probe:Destroy()

		DEFAULT_TRANSPORT_PROPERTY_CACHE[className] = defaults
		DEFAULT_TRANSPORT_FAST_COMPARE_CACHE[className] = fastDefaults
		return defaults
	end

	local function getDefaultTransportFastCompareProperties(className: string): any
		local cached = DEFAULT_TRANSPORT_FAST_COMPARE_CACHE[className]
		if cached ~= nil then
			if cached == NO_DEFAULTS then
				return nil
			end
			return cached
		end
		getDefaultTransportProperties(className)
		cached = DEFAULT_TRANSPORT_FAST_COMPARE_CACHE[className]
		if cached == nil or cached == NO_DEFAULTS then
			return nil
		end
		return cached
	end

	local function sanitizePropertySchemaEntry(className: string, rawEntry: any): { any }?
		if type(rawEntry) == "string" then
			local normalized = normalizePropertyName(rawEntry)
			if normalized == "" then
				error("Property candidate names must not be empty")
			end
			if shouldSkipStructuralTransportProperty(className, normalized) then
				return nil
			end
			return { normalized, COMPACT_TYPE_IDS.String, false }
		end
		if type(rawEntry) ~= "table" then
			error("Property candidate entries must be strings or arrays")
		end

		local rawName = rawEntry[1]
		local rawTypeId = rawEntry[2]
		if type(rawName) ~= "string" or rawName == "" then
			error("Property candidate names must be non-empty strings")
		end
		if
			type(rawTypeId) ~= "number"
			or rawTypeId ~= rawTypeId
			or rawTypeId % 1 ~= 0
			or rawTypeId < COMPACT_TYPE_IDS.Bool
			or rawTypeId > COMPACT_TYPE_IDS.Ray
		then
			error("Property candidate type IDs must be supported integers")
		end

		local normalized = normalizePropertyName(rawName)
		if normalized == "" then
			error("Property candidate names must not be empty")
		end
		if shouldSkipStructuralTransportProperty(className, normalized) then
			return nil
		end

		local rawEnumType = rawEntry[3]
		if rawEnumType ~= nil and rawEnumType ~= false and (type(rawEnumType) ~= "string" or rawEnumType == "") then
			error("Property candidate enum types must be non-empty strings")
		end
		return { normalized, rawTypeId, if type(rawEnumType) == "string" then rawEnumType else false }
	end

	local function sanitizePropertySchema(className: string, entries: { any }): { any }
		local numericEntryCount = 0
		for key in pairs(entries) do
			if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then
				error("Property candidate class entries must be dense arrays")
			end
			numericEntryCount += 1
		end
		if numericEntryCount == 0 or numericEntryCount ~= #entries then
			error("Property candidate class entries must be non-empty dense arrays")
		end

		local sanitizedSchema = {}
		local seen: { [string]: boolean } = {}
		for _, rawEntry in ipairs(entries) do
			local schemaEntry = sanitizePropertySchemaEntry(className, rawEntry)
			if schemaEntry then
				local key = propertyKey(schemaEntry[1])
				if not seen[key] then
					seen[key] = true
					sanitizedSchema[#sanitizedSchema + 1] = schemaEntry
				end
			end
		end
		return sanitizedSchema
	end

	local function configurePropertyCandidates(payload: any): { [string]: any }
		if type(payload) ~= "table" then
			error("configurePropertyCandidates expects table payload")
		end

		local configuredSchemas = {}
		local configuredClassCount = 0
		local configuredPropertyCount = 0
		for className, names in pairs(payload) do
			if type(className) ~= "string" or className == "" or type(names) ~= "table" then
				error("Property candidate classes must map non-empty class names to arrays")
			end
			local sanitizedSchema = sanitizePropertySchema(className, names)
			if #sanitizedSchema > 0 then
				configuredSchemas[className] = sanitizedSchema
				configuredClassCount += 1
				configuredPropertyCount += #sanitizedSchema
			end
		end
		if configuredPropertyCount == 0 then
			error("Property candidate payload contains no usable properties")
		end

		EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS =
			PropertySchemaModule.mergeSchemas(BUNDLED_PROPERTY_SCHEMAS_BY_CLASS, configuredSchemas)
		EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS =
			PropertySchemaModule.buildCandidatesFromSchemas(EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS)
		exportPropertyRelevant = PropertySchemaModule.makeExportPropertyFilter(RbxDomDatabase, EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS)
		DEFAULT_PROPERTY_CACHE = {}
		DEFAULT_TRANSPORT_PROPERTY_CACHE = {}
		DEFAULT_TRANSPORT_FAST_COMPARE_CACHE = {}
		CLASS_PROPERTY_CANDIDATES_CACHE = {}
		CLASS_PROPERTY_SCHEMA_CACHE = {}
		for serviceName in pairs(stateByService) do
			stateByService[serviceName] = nil
		end

		Config.studioChanges.configurePropertyCandidates(EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS)
		local classCount, propertyCount = PropertySchemaModule.countCandidates(EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS)

		return {
			ok = true,
			classCount = classCount,
			propertyCount = propertyCount,
			configuredClassCount = configuredClassCount,
			configuredPropertyCount = configuredPropertyCount,
		}
	end

	local function configureExportOptions(payload: any): { [string]: any }
		if type(payload) ~= "table" then
			error("setExportOptions expects an object")
		end
		for key, value in pairs(payload) do
			if key ~= "exportAllProperties" then
				error("Unknown export option " .. tostring(key))
			end
			if type(value) ~= "boolean" then
				error(key .. " must be a boolean")
			end
		end
		if payload.exportAllProperties ~= nil and payload.exportAllProperties ~= EXPORT_ALL_PROPERTIES then
			EXPORT_ALL_PROPERTIES = payload.exportAllProperties
			table.clear(CLASS_PROPERTY_SCHEMA_CACHE)
			table.clear(CLASS_PROPERTY_CANDIDATES_CACHE)
			for _, serviceState in pairs(stateByService) do
				serviceState.hotPropertySchemaByClass = nil
			end
		end
		plugin:SetSetting(SETTINGS_PREFIX .. "exportAllProperties", EXPORT_ALL_PROPERTIES)
		Config.updateStatusText()
		return { exportAllProperties = EXPORT_ALL_PROPERTIES }
	end

	getClassPropertySchema = function(className: string): { { any } }?
		local cached = CLASS_PROPERTY_SCHEMA_CACHE[className]
		if cached == nil then
			local external = EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS[className]
			if external ~= nil and #external > 0 then
				local ok, probe = pcall(Instance.new, className)
				if ok and probe ~= nil then
					local validated = {}
					for _, schemaEntry in ipairs(external) do
						local propertyName = schemaEntry[1]
						if not shouldSkipStructuralTransportProperty(className, propertyName) then
							local readable = tryRead(probe, propertyName)
							if readable then
								validated[#validated + 1] = { schemaEntry[1], schemaEntry[2], schemaEntry[3] }
							end
						end
					end
					probe:Destroy()
					if #validated > 0 then
						CLASS_PROPERTY_SCHEMA_CACHE[className] = validated
						cached = validated
					else
						CLASS_PROPERTY_SCHEMA_CACHE[className] = NO_PROPERTIES
						cached = NO_PROPERTIES
					end
				else
					CLASS_PROPERTY_SCHEMA_CACHE[className] = external
					cached = external
				end
			elseif EXPORT_ALL_PROPERTIES then
				CLASS_PROPERTY_SCHEMA_CACHE[className] = NO_PROPERTIES
				cached = NO_PROPERTIES
			else
				getDefaultSerializedProperties(className)
				cached = CLASS_PROPERTY_SCHEMA_CACHE[className]
			end
		end
		if cached == nil or cached == NO_PROPERTIES then
			return nil
		end
		return cached
	end

	local function getClassPropertyCandidates(className: string): { string }?
		local cached = CLASS_PROPERTY_CANDIDATES_CACHE[className]
		if cached == nil then
			local schema = getClassPropertySchema(className)
			if schema ~= nil then
				local names = table.create(#schema)
				for i, schemaEntry in ipairs(schema) do
					names[i] = schemaEntry[1]
				end
				CLASS_PROPERTY_CANDIDATES_CACHE[className] = names
				cached = names
			else
				CLASS_PROPERTY_CANDIDATES_CACHE[className] = NO_PROPERTIES
				cached = NO_PROPERTIES
			end
		end
		if cached == nil or cached == NO_PROPERTIES then
			return nil
		end
		return cached
	end

	local function ensureScriptRangeIndex(state: ServiceState)
		if state.scriptIndices and state.scriptInstancesByIndex then
			return
		end
		local scriptIndices = table.create(#state.scriptObjects)
		local scriptInstancesByIndex = {}
		for _, inst in ipairs(state.scriptObjects) do
			local sourceIndex = IdentityModule.getCachedInstanceIndex(state, inst)
			if sourceIndex then
				scriptIndices[#scriptIndices + 1] = sourceIndex
				scriptInstancesByIndex[sourceIndex] = inst
			end
		end
		state.scriptIndices = scriptIndices
		state.scriptInstancesByIndex = scriptInstancesByIndex
	end

	local function getServicePropertySchema(state: ServiceState): { [string]: { { any } } }
		local cached = state.servicePropertySchemaByClass
		if cached ~= nil then
			return cached
		end

		local byClass = {}
		for _, className in ipairs(state.classNames) do
			local sourceSchema = getClassPropertySchema(className) or {}
			local schemaEntries = table.create(#sourceSchema)
			for i, schemaEntry in ipairs(sourceSchema) do
				schemaEntries[i] = { schemaEntry[1], schemaEntry[2], schemaEntry[3] or false }
			end
			byClass[className] = schemaEntries
		end

		state.servicePropertySchemaByClass = byClass
		return byClass
	end

	function Config.captureEditorBinaryRootProperties(serviceName: string): { [string]: any }
		local service = game:GetService(serviceName)
		local properties = {}
		for _, propertyName in ipairs(getClassPropertyCandidates(service.ClassName) or {}) do
			local okRead, value = tryRead(service, propertyName)
			if okRead then
				properties[propertyName] = value
			end
		end
		if serviceName == "Workspace" then
			properties.CollisionGroupData = CollisionGroupsModule.read()
		end
		return properties
	end

	function Config.readEditorBinaryRootProperties(serviceName: string, state: ServiceState): { [string]: any }
		if state.nativeRootProperties ~= nil then
			return state.nativeRootProperties
		end
		local service = game:GetService(serviceName)
		local candidates = getClassPropertyCandidates(state.rootClassName or service.ClassName)
		local properties = {}
		if candidates == nil then
			if state.nativeSnapshotRoot then
				state.nativeRootProperties = properties
			end
			return properties
		end
		for _, propertyName in ipairs(candidates) do
			if serviceName == "Workspace" and propertyName == "CollisionGroupData" then
				continue
			end
			local okRead, value
			if state.nativeRootPropertyValues ~= nil then
				value = state.nativeRootPropertyValues[propertyName]
				okRead = value ~= nil
			else
				okRead, value = tryRead(service, propertyName)
			end
			if okRead then
				local serialized = serializeValue(value, state)
				if serialized ~= nil then
					properties[propertyName] = serialized
				end
			end
		end
		if serviceName == "Workspace" then
			local collisionGroups = if state.nativeRootPropertyValues ~= nil
				then state.nativeRootPropertyValues.CollisionGroupData
				else CollisionGroupsModule.read()
			properties.CollisionGroupData = {
				_type = "BinaryString",
				base64 = buffer.tostring(EncodingService:Base64Encode(buffer.fromstring(collisionGroups))),
			}
			-- Persist the viewport role as a reference, never as an editable setting.
			-- Names and sibling ordinals cannot identify it after Studio reopens.
			properties.CurrentCamera = serializeValue(service.CurrentCamera, state)
		end
		if state.nativeSnapshotRoot then
			state.nativeRootProperties = properties
		end
		return properties
	end

	local function enumDatabaseName(enumType: string): string
		local prefix = "Enum."
		if string.sub(enumType, 1, #prefix) == prefix then
			return string.sub(enumType, #prefix + 1)
		end
		return enumType
	end

	local function getEnumValueNames(enumType: string): any
		local cached = ENUM_VALUE_NAMES_BY_TYPE_CACHE[enumType]
		if cached ~= nil then
			return cached
		end

		local enumName = enumDatabaseName(enumType)
		local enums = type(RbxDomDatabase) == "table" and RbxDomDatabase.Enums or nil
		local enumData = type(enums) == "table" and enums[enumName] or nil
		local items = type(enumData) == "table" and enumData.items or nil

		local out = {}
		if type(items) == "table" then
			for name, value in pairs(items) do
				if type(name) == "string" and type(value) == "number" then
					out[tostring(value)] = name
				end
			end
		end
		if not next(out) then
			local okEnum, liveItems = pcall(function()
				return (Enum :: any)[enumName]:GetEnumItems()
			end)
			if okEnum and type(liveItems) == "table" then
				for _, item in ipairs(liveItems) do
					out[tostring(item.Value)] = item.Name
				end
			end
		end
		if not next(out) then
			ENUM_VALUE_NAMES_BY_TYPE_CACHE[enumType] = false
			return false
		end

		ENUM_VALUE_NAMES_BY_TYPE_CACHE[enumType] = out
		return out
	end

	Config.getEditorBinaryEnumValueNames = getEnumValueNames

	local function getServiceEnumValueNamesByType(state: ServiceState): { [string]: any }
		local out = {}
		for _, className in ipairs(state.classNames) do
			local sourceSchema = getClassPropertySchema(className) or {}
			for _, schemaEntry in ipairs(sourceSchema) do
				if schemaEntry[2] == COMPACT_TYPE_IDS.EnumItem and type(schemaEntry[3]) == "string" then
					local enumType = schemaEntry[3]
					if out[enumType] == nil then
						local valueNames = getEnumValueNames(enumType)
						if valueNames then
							out[enumType] = valueNames
						end
					end
				end
			end
		end
		return out
	end

	function Config.getHotPropertySchema(state: ServiceState, className: string): { [string]: any }
		local cachedByClass = state.hotPropertySchemaByClass
		if cachedByClass == nil then
			cachedByClass = {}
			state.hotPropertySchemaByClass = cachedByClass
		end
		local cached = cachedByClass[className]
		if cached ~= nil then
			return cached
		end

		local sourceSchema = getClassPropertySchema(className) or {}
		local propertyCount = #sourceSchema
		local defaultProperties = getDefaultTransportProperties(className)
		local defaultFastCompareProperties = getDefaultTransportFastCompareProperties(className)
		local names = table.create(propertyCount)
		local typeIds = table.create(propertyCount)
		local enumTypes = table.create(propertyCount)
		local defaults = table.create(propertyCount)
		local fastDefaults = table.create(propertyCount)
		local maskWordIndices = table.create(propertyCount)
		local maskBitValues = table.create(propertyCount)
		local fastCompareModes = table.create(propertyCount)
		local compareFns = table.create(propertyCount)
		local encodeFns = table.create(propertyCount)
		local skipEncode = table.create(propertyCount)
		for i, schemaEntry in ipairs(sourceSchema) do
			local propertyName = schemaEntry[1]
			local typeId = schemaEntry[2]
			local enumType = if type(schemaEntry[3]) == "string" then schemaEntry[3] else false
			local defaultComparable = if defaultProperties then defaultProperties[propertyName] else nil
			local defaultFastComparable = if defaultFastCompareProperties
				then defaultFastCompareProperties[propertyName]
				else defaultComparable
			names[i] = propertyName
			typeIds[i] = typeId
			enumTypes[i] = enumType
			defaults[i] = defaultComparable
			fastDefaults[i] = defaultFastComparable
			maskWordIndices[i] = math.floor((i - 1) / 31) + 1
			maskBitValues[i] = bit32.lshift(1, (i - 1) % 31)
			if defaultComparable ~= nil then
				if
					typeId == COMPACT_TYPE_IDS.Bool
					or typeId == COMPACT_TYPE_IDS.Number
					or typeId == COMPACT_TYPE_IDS.String
					or typeId == COMPACT_TYPE_IDS.ContentId
					or typeId == COMPACT_TYPE_IDS.BinaryString
				then
					fastCompareModes[i] = FAST_COMPARE_EQUAL
				elseif type(defaultComparable) == "table" then
					if typeId == COMPACT_TYPE_IDS.Vector2 then
						fastCompareModes[i] = FAST_COMPARE_VECTOR2
					elseif typeId == COMPACT_TYPE_IDS.Vector3 then
						fastCompareModes[i] = FAST_COMPARE_VECTOR3
					elseif typeId == COMPACT_TYPE_IDS.UDim then
						fastCompareModes[i] = FAST_COMPARE_UDIM
					elseif typeId == COMPACT_TYPE_IDS.UDim2 then
						fastCompareModes[i] = FAST_COMPARE_UDIM2
					elseif typeId == COMPACT_TYPE_IDS.Color3 then
						fastCompareModes[i] = FAST_COMPARE_COLOR3
					elseif typeId == COMPACT_TYPE_IDS.CFrame and #defaultComparable == 12 then
						fastCompareModes[i] = FAST_COMPARE_CFRAME
					elseif typeId == COMPACT_TYPE_IDS.Rect then
						fastCompareModes[i] = FAST_COMPARE_RECT
					end
				elseif typeId == COMPACT_TYPE_IDS.BrickColor then
					fastCompareModes[i] = FAST_COMPARE_BRICKCOLOR
				elseif typeId == COMPACT_TYPE_IDS.EnumItem then
					fastCompareModes[i] = FAST_COMPARE_ENUM_VALUE
				end
			end
			compareFns[i] = Config.compareDefaultValueV5ByTypeId[typeId] or false
			encodeFns[i] = Config.encodeValueV5ByTypeId[typeId] or false
			skipEncode[i] = className == "Texture" and propertyName == "Rotation"
		end

		local hotSchema = {
			className = className,
			count = propertyCount,
			maxMaskWords = math.ceil(propertyCount / 31),
			names = names,
			typeIds = typeIds,
			enumTypes = enumTypes,
			defaults = defaults,
			fastDefaults = fastDefaults,
			maskWordIndices = maskWordIndices,
			maskBitValues = maskBitValues,
			fastCompareModes = fastCompareModes,
			compareFns = compareFns,
			encodeFns = encodeFns,
			skipEncode = skipEncode,
			exporter = false,
			exporterWithFallback = false,
			fallbacksLearned = false,
			usesFallbackMap = false,
		}
		cachedByClass[className] = hotSchema
		return hotSchema
	end

	function Config.learnClassPropertyFallbacks(
		state: ServiceState,
		instance: Instance,
		className: string,
		propertyNames: { string }
	): boolean
		local fallbackMap = Config.getClassPropertyFallbackMap(state, className)
		local learned = false
		for _, propertyName in ipairs(propertyNames) do
			if not fallbackMap[propertyName] then
				local ok = pcall(function()
					return (instance :: any)[propertyName]
				end)
				if not ok then
					fallbackMap[propertyName] = true
					learned = true
				end
			end
		end
		return learned
	end

	function Config.internBatchString(strings: { string }, stringIds: { [string]: number }, text: string): number
		local existing = stringIds[text]
		if existing ~= nil then
			return existing
		end
		local nextId = #strings + 1
		strings[nextId] = text
		stringIds[text] = nextId
		return nextId
	end

	local function encodeComparableRefValue(state: ServiceState?, instance: Instance): any
		if state ~= nil then
			local instanceIndex = IdentityModule.getCachedInstanceIndex(state, instance)
			if instanceIndex ~= nil then
				return instanceIndex
			end
		end

		local pathSegments, pathOrdinals
		if state ~= nil then
			pathSegments, pathOrdinals = IdentityModule.getCachedRefPathParts(state, instance)
		else
			pathSegments, pathOrdinals = IdentityModule.getRefPathParts(instance)
		end
		if pathSegments == nil or pathOrdinals == nil or #pathSegments == 0 then
			return nil
		end

		local out = table.create(#pathSegments + 3)
		out[1] = 0
		local debugId = if state ~= nil
			then IdentityModule.getCachedDebugId(state, instance)
			else IdentityModule.getDebugId(instance)
		out[2] = debugId or false
		out[3] = pathOrdinals
		for i, segment in ipairs(pathSegments) do
			out[i + 3] = segment
		end
		return out
	end

	encodeSchemaComparableValue = function(typeId: number, _enumType: string?, value: any, state: ServiceState?): any
		if typeId == COMPACT_TYPE_IDS.Bool then
			if type(value) == "boolean" then
				return value
			end
		elseif typeId == COMPACT_TYPE_IDS.Number then
			if type(value) == "number" then
				return ValueCodecModule.encodeTransportNumber(value)
			end
		elseif typeId == COMPACT_TYPE_IDS.String or typeId == COMPACT_TYPE_IDS.BinaryString then
			if type(value) == "string" then
				return value
			end
		elseif typeId == COMPACT_TYPE_IDS.ContentId then
			return serializeContentValue(value)
		elseif typeId == COMPACT_TYPE_IDS.Vector2 and typeof(value) == "Vector2" then
			return ValueCodecModule.encodeTransportComponents(value.X, value.Y)
		elseif typeId == COMPACT_TYPE_IDS.Vector3 and typeof(value) == "Vector3" then
			return ValueCodecModule.encodeTransportComponents(value.X, value.Y, value.Z)
		elseif typeId == COMPACT_TYPE_IDS.UDim and typeof(value) == "UDim" then
			return ValueCodecModule.encodeTransportComponents(value.Scale, value.Offset)
		elseif typeId == COMPACT_TYPE_IDS.UDim2 and typeof(value) == "UDim2" then
			return ValueCodecModule.encodeTransportComponents(
				value.X.Scale,
				value.X.Offset,
				value.Y.Scale,
				value.Y.Offset
			)
		elseif typeId == COMPACT_TYPE_IDS.Color3 and typeof(value) == "Color3" then
			return ValueCodecModule.encodeTransportComponents(value.R, value.G, value.B)
		elseif typeId == COMPACT_TYPE_IDS.BrickColor and typeof(value) == "BrickColor" then
			return value.Number
		elseif typeId == COMPACT_TYPE_IDS.NumberRange and typeof(value) == "NumberRange" then
			return ValueCodecModule.encodeTransportComponents(value.Min, value.Max)
		elseif typeId == COMPACT_TYPE_IDS.PhysicalProperties then
			if value == false then
				return false
			end
			local comparable = physicalPropertiesComparable(value)
			return if comparable
				then ValueCodecModule.encodeTransportComponents(table.unpack(comparable, 1, 6))
				else nil
		elseif typeId == COMPACT_TYPE_IDS.EnumItem and typeof(value) == "EnumItem" then
			return value.Name
		elseif typeId == COMPACT_TYPE_IDS.CFrame and typeof(value) == "CFrame" then
			return ValueCodecModule.encodeTransportComponents(value:GetComponents())
		elseif typeId == COMPACT_TYPE_IDS.Rect and typeof(value) == "Rect" then
			return ValueCodecModule.encodeTransportComponents(value.Min.X, value.Min.Y, value.Max.X, value.Max.Y)
		elseif typeId == COMPACT_TYPE_IDS.Font and typeof(value) == "Font" then
			return { value.Family, tostring(value.Weight), tostring(value.Style) }
		elseif typeId == COMPACT_TYPE_IDS.ColorSequence and typeof(value) == "ColorSequence" then
			local out = table.create(#value.Keypoints * 4)
			local writeIndex = 1
			for _, keypoint in ipairs(value.Keypoints) do
				local components = ValueCodecModule.encodeTransportComponents(
					keypoint.Time,
					keypoint.Value.R,
					keypoint.Value.G,
					keypoint.Value.B
				)
				out[writeIndex] = components[1]
				out[writeIndex + 1] = components[2]
				out[writeIndex + 2] = components[3]
				out[writeIndex + 3] = components[4]
				writeIndex += 4
			end
			return out
		elseif typeId == COMPACT_TYPE_IDS.NumberSequence and typeof(value) == "NumberSequence" then
			local out = table.create(#value.Keypoints * 3)
			local writeIndex = 1
			for _, keypoint in ipairs(value.Keypoints) do
				local components =
					ValueCodecModule.encodeTransportComponents(keypoint.Time, keypoint.Value, keypoint.Envelope)
				out[writeIndex] = components[1]
				out[writeIndex + 1] = components[2]
				out[writeIndex + 2] = components[3]
				writeIndex += 3
			end
			return out
		elseif typeId == COMPACT_TYPE_IDS.Axes and typeof(value) == "Axes" then
			return axesBitmask(value)
		elseif typeId == COMPACT_TYPE_IDS.Faces and typeof(value) == "Faces" then
			return facesBitmask(value)
		elseif typeId == COMPACT_TYPE_IDS.Ray and typeof(value) == "Ray" then
			return ValueCodecModule.encodeTransportComponents(
				value.Origin.X,
				value.Origin.Y,
				value.Origin.Z,
				value.Direction.X,
				value.Direction.Y,
				value.Direction.Z
			)
		elseif typeId == COMPACT_TYPE_IDS.Ref and typeof(value) == "Instance" then
			return encodeComparableRefValue(state, value)
		end

		return nil
	end

	Config.encodeNumberV5 = ValueCodecModule.encodeTransportNumber

	Config.compareDefaultValueV5ByTypeId = {
		[COMPACT_TYPE_IDS.Bool] = function(value: any, defaultComparable: any): boolean
			return type(value) == "boolean" and value == defaultComparable
		end,
		[COMPACT_TYPE_IDS.Number] = function(value: any, defaultComparable: any): boolean
			return type(value) == "number" and value == defaultComparable
		end,
		[COMPACT_TYPE_IDS.String] = function(value: any, defaultComparable: any): boolean
			return type(value) == "string" and value == defaultComparable
		end,
		[COMPACT_TYPE_IDS.ContentId] = function(value: any, defaultComparable: any): boolean
			return serializeContentValue(value) == defaultComparable
		end,
		[COMPACT_TYPE_IDS.BinaryString] = function(value: any, defaultComparable: any): boolean
			return type(value) == "string" and value == defaultComparable
		end,
		[COMPACT_TYPE_IDS.Vector2] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "Vector2"
				and type(defaultComparable) == "table"
				and value.X == defaultComparable[1]
				and value.Y == defaultComparable[2]
		end,
		[COMPACT_TYPE_IDS.Vector3] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "Vector3"
				and type(defaultComparable) == "table"
				and value.X == defaultComparable[1]
				and value.Y == defaultComparable[2]
				and value.Z == defaultComparable[3]
		end,
		[COMPACT_TYPE_IDS.UDim] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "UDim"
				and type(defaultComparable) == "table"
				and value.Scale == defaultComparable[1]
				and value.Offset == defaultComparable[2]
		end,
		[COMPACT_TYPE_IDS.UDim2] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "UDim2"
				and type(defaultComparable) == "table"
				and value.X.Scale == defaultComparable[1]
				and value.X.Offset == defaultComparable[2]
				and value.Y.Scale == defaultComparable[3]
				and value.Y.Offset == defaultComparable[4]
		end,
		[COMPACT_TYPE_IDS.Color3] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "Color3"
				and type(defaultComparable) == "table"
				and value.R == defaultComparable[1]
				and value.G == defaultComparable[2]
				and value.B == defaultComparable[3]
		end,
		[COMPACT_TYPE_IDS.BrickColor] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "BrickColor" and value.Number == defaultComparable
		end,
		[COMPACT_TYPE_IDS.NumberRange] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "NumberRange"
				and type(defaultComparable) == "table"
				and value.Min == defaultComparable[1]
				and value.Max == defaultComparable[2]
		end,
		[COMPACT_TYPE_IDS.PhysicalProperties] = function(value: any, defaultComparable: any): boolean
			if defaultComparable == false then
				return value == false
			end
			local comparable = physicalPropertiesComparable(value)
			return comparable ~= nil and deepEqual(comparable, defaultComparable)
		end,
		[COMPACT_TYPE_IDS.EnumItem] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "EnumItem" and value.Name == defaultComparable
		end,
		[COMPACT_TYPE_IDS.CFrame] = function(value: any, defaultComparable: any): boolean
			if typeof(value) ~= "CFrame" or type(defaultComparable) ~= "table" or #defaultComparable ~= 12 then
				return false
			end
			local c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11 = value:GetComponents()
			return c0 == defaultComparable[1]
				and c1 == defaultComparable[2]
				and c2 == defaultComparable[3]
				and c3 == defaultComparable[4]
				and c4 == defaultComparable[5]
				and c5 == defaultComparable[6]
				and c6 == defaultComparable[7]
				and c7 == defaultComparable[8]
				and c8 == defaultComparable[9]
				and c9 == defaultComparable[10]
				and c10 == defaultComparable[11]
				and c11 == defaultComparable[12]
		end,
		[COMPACT_TYPE_IDS.Rect] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "Rect"
				and type(defaultComparable) == "table"
				and value.Min.X == defaultComparable[1]
				and value.Min.Y == defaultComparable[2]
				and value.Max.X == defaultComparable[3]
				and value.Max.Y == defaultComparable[4]
		end,
		[COMPACT_TYPE_IDS.Font] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "Font"
				and type(defaultComparable) == "table"
				and value.Family == defaultComparable[1]
				and tostring(value.Weight) == defaultComparable[2]
				and tostring(value.Style) == defaultComparable[3]
		end,
		[COMPACT_TYPE_IDS.ColorSequence] = function(value: any, defaultComparable: any): boolean
			if typeof(value) ~= "ColorSequence" or type(defaultComparable) ~= "table" then
				return false
			end
			local keypoints = value.Keypoints
			if #keypoints * 4 ~= #defaultComparable then
				return false
			end
			local writeIndex = 1
			for _, keypoint in ipairs(keypoints) do
				if
					keypoint.Time ~= defaultComparable[writeIndex]
					or keypoint.Value.R ~= defaultComparable[writeIndex + 1]
					or keypoint.Value.G ~= defaultComparable[writeIndex + 2]
					or keypoint.Value.B ~= defaultComparable[writeIndex + 3]
				then
					return false
				end
				writeIndex += 4
			end
			return true
		end,
		[COMPACT_TYPE_IDS.NumberSequence] = function(value: any, defaultComparable: any): boolean
			if typeof(value) ~= "NumberSequence" or type(defaultComparable) ~= "table" then
				return false
			end
			local keypoints = value.Keypoints
			if #keypoints * 3 ~= #defaultComparable then
				return false
			end
			local writeIndex = 1
			for _, keypoint in ipairs(keypoints) do
				if
					keypoint.Time ~= defaultComparable[writeIndex]
					or keypoint.Value ~= defaultComparable[writeIndex + 1]
					or keypoint.Envelope ~= defaultComparable[writeIndex + 2]
				then
					return false
				end
				writeIndex += 3
			end
			return true
		end,
		[COMPACT_TYPE_IDS.Axes] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "Axes" and axesBitmask(value) == defaultComparable
		end,
		[COMPACT_TYPE_IDS.Faces] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "Faces" and facesBitmask(value) == defaultComparable
		end,
		[COMPACT_TYPE_IDS.Ray] = function(value: any, defaultComparable: any): boolean
			return typeof(value) == "Ray"
				and type(defaultComparable) == "table"
				and value.Origin.X == defaultComparable[1]
				and value.Origin.Y == defaultComparable[2]
				and value.Origin.Z == defaultComparable[3]
				and value.Direction.X == defaultComparable[4]
				and value.Direction.Y == defaultComparable[5]
				and value.Direction.Z == defaultComparable[6]
		end,
		[COMPACT_TYPE_IDS.Ref] = function(value: any, defaultComparable: any, state: ServiceState?): boolean
			return typeof(value) == "Instance"
				and deepEqual(encodeSchemaComparableValue(COMPACT_TYPE_IDS.Ref, nil, value, state), defaultComparable)
		end,
	}

	Config.encodeValueV5ByTypeId = {
		[COMPACT_TYPE_IDS.Bool] = function(value: any): any
			if type(value) == "boolean" then
				return value
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.Number] = function(value: any): any
			return Config.encodeNumberV5(value)
		end,
		[COMPACT_TYPE_IDS.String] = function(
			value: any,
			_state: ServiceState?,
			strings: { string },
			stringIds: { [string]: number }
		): any
			if type(value) == "string" then
				return Config.internBatchString(strings, stringIds, value)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.ContentId] = function(
			value: any,
			_state: ServiceState?,
			strings: { string },
			stringIds: { [string]: number }
		): any
			local serialized = serializeContentValue(value)
			if serialized then
				return Config.internBatchString(strings, stringIds, serialized)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.BinaryString] = function(
			value: any,
			_state: ServiceState?,
			strings: { string },
			stringIds: { [string]: number }
		): any
			if type(value) == "string" then
				return Config.internBatchString(strings, stringIds, value)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.Vector2] = function(value: any): any
			if typeof(value) == "Vector2" then
				return ValueCodecModule.encodeTransportComponents(value.X, value.Y)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.Vector3] = function(value: any): any
			if typeof(value) == "Vector3" then
				return ValueCodecModule.encodeTransportComponents(value.X, value.Y, value.Z)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.UDim] = function(value: any): any
			if typeof(value) == "UDim" then
				return ValueCodecModule.encodeTransportComponents(value.Scale, value.Offset)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.UDim2] = function(value: any): any
			if typeof(value) == "UDim2" then
				return ValueCodecModule.encodeTransportComponents(
					value.X.Scale,
					value.X.Offset,
					value.Y.Scale,
					value.Y.Offset
				)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.Color3] = function(value: any): any
			if typeof(value) == "Color3" then
				return ValueCodecModule.encodeTransportComponents(value.R, value.G, value.B)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.BrickColor] = function(value: any): any
			if typeof(value) == "BrickColor" then
				return value.Number
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.NumberRange] = function(value: any): any
			if typeof(value) == "NumberRange" then
				return ValueCodecModule.encodeTransportComponents(value.Min, value.Max)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.PhysicalProperties] = function(value: any): any
			if value == false then
				return false
			end
			local comparable = physicalPropertiesComparable(value)
			return if comparable
				then ValueCodecModule.encodeTransportComponents(table.unpack(comparable, 1, 6))
				else nil
		end,
		[COMPACT_TYPE_IDS.EnumItem] = function(
			value: any,
			_state: ServiceState?,
			_strings: { string },
			_stringIds: { [string]: number }
		): any
			if typeof(value) == "EnumItem" then
				return value.Value
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.CFrame] = function(value: any): any
			if typeof(value) == "CFrame" then
				return ValueCodecModule.encodeTransportComponents(value:GetComponents())
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.Rect] = function(value: any): any
			if typeof(value) == "Rect" then
				return ValueCodecModule.encodeTransportComponents(value.Min.X, value.Min.Y, value.Max.X, value.Max.Y)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.Font] = function(
			value: any,
			_state: ServiceState?,
			strings: { string },
			stringIds: { [string]: number }
		): any
			if typeof(value) == "Font" then
				return {
					Config.internBatchString(strings, stringIds, value.Family),
					Config.internBatchString(strings, stringIds, tostring(value.Weight)),
					Config.internBatchString(strings, stringIds, tostring(value.Style)),
				}
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.ColorSequence] = function(value: any): any
			if typeof(value) ~= "ColorSequence" then
				return nil
			end
			local keypoints = value.Keypoints
			local out = table.create(#keypoints * 4)
			local writeIndex = 1
			for _, keypoint in ipairs(keypoints) do
				local components = ValueCodecModule.encodeTransportComponents(
					keypoint.Time,
					keypoint.Value.R,
					keypoint.Value.G,
					keypoint.Value.B
				)
				out[writeIndex] = components[1]
				out[writeIndex + 1] = components[2]
				out[writeIndex + 2] = components[3]
				out[writeIndex + 3] = components[4]
				writeIndex += 4
			end
			return out
		end,
		[COMPACT_TYPE_IDS.NumberSequence] = function(value: any): any
			if typeof(value) ~= "NumberSequence" then
				return nil
			end
			local keypoints = value.Keypoints
			local out = table.create(#keypoints * 3)
			local writeIndex = 1
			for _, keypoint in ipairs(keypoints) do
				local components =
					ValueCodecModule.encodeTransportComponents(keypoint.Time, keypoint.Value, keypoint.Envelope)
				out[writeIndex] = components[1]
				out[writeIndex + 1] = components[2]
				out[writeIndex + 2] = components[3]
				writeIndex += 3
			end
			return out
		end,
		[COMPACT_TYPE_IDS.Axes] = function(value: any): any
			return if typeof(value) == "Axes" then axesBitmask(value) else nil
		end,
		[COMPACT_TYPE_IDS.Faces] = function(value: any): any
			return if typeof(value) == "Faces" then facesBitmask(value) else nil
		end,
		[COMPACT_TYPE_IDS.Ray] = function(value: any): any
			if typeof(value) ~= "Ray" then
				return nil
			end
			return ValueCodecModule.encodeTransportComponents(
				value.Origin.X,
				value.Origin.Y,
				value.Origin.Z,
				value.Direction.X,
				value.Direction.Y,
				value.Direction.Z
			)
		end,
		[COMPACT_TYPE_IDS.Ref] = function(
			value: any,
			state: ServiceState?,
			strings: { string },
			stringIds: { [string]: number }
		): any
			if typeof(value) ~= "Instance" then
				return nil
			end
			local comparable = encodeComparableRefValue(state, value)
			if comparable == nil then
				return nil
			end
			if type(comparable) == "number" then
				return comparable
			end
			local out = table.create(#comparable)
			out[1] = 0
			out[2] = if type(comparable[2]) == "string"
				then Config.internBatchString(strings, stringIds, comparable[2])
				else false
			out[3] = comparable[3]
			for i = 4, #comparable do
				out[i] = Config.internBatchString(strings, stringIds, comparable[i])
			end
			return out
		end,
	}

	local function compactPropertyMask(maskWords: { number }?, maskWordCount: number): any
		if maskWords == nil or maskWordCount == 0 then
			return false
		end
		if maskWordCount == 1 then
			return maskWords[1] or 0
		end
		local denseMask = table.create(maskWordCount)
		for index = 1, maskWordCount do
			denseMask[index] = maskWords[index] or 0
		end
		return denseMask
	end

	local function buildCompactV5Row(
		state: ServiceState,
		instance: Instance,
		instanceIndex: number,
		classValue: any,
		parentIndex: number?,
		attributes: any,
		propertyMask: any,
		propertyValues: any,
		compactOverlay: boolean?,
		strings: { string },
		stringIds: { [string]: number },
		internString: (any, any, string) -> number
	): any
		local hasProperties = propertyMask or propertyValues
		if compactOverlay then
			if not hasProperties then
				return if attributes then { classValue, attributes } else false
			end
			return if attributes
				then { classValue, attributes, propertyMask, propertyValues }
				else { classValue, false, propertyMask, propertyValues }
		end

		local nameId = internString(strings, stringIds, state.nameByIndex[instanceIndex] or instance.Name)
		local parentValue = parentIndex or false
		if not hasProperties then
			return if attributes
				then { nameId, classValue, parentValue, attributes }
				else { nameId, classValue, parentValue }
		end
		return if attributes
			then { nameId, classValue, parentValue, attributes, propertyMask, propertyValues }
			else { nameId, classValue, parentValue, propertyMask, propertyValues }
	end

	function Config.buildCompactV5Exporter(className: string, hotSchema: { [string]: any }, useFallbackMap: boolean?)
		local propertyCount = hotSchema.count
		local propertyNames = hotSchema.names
		local typeIds = hotSchema.typeIds
		local defaults = hotSchema.defaults
		local fastDefaults = hotSchema.fastDefaults
		local maskWordIndices = hotSchema.maskWordIndices
		local maskBitValues = hotSchema.maskBitValues
		local fastCompareModes = hotSchema.fastCompareModes
		local compareFns = hotSchema.compareFns
		local encodeFns = hotSchema.encodeFns
		local skipEncode = hotSchema.skipEncode
		local shouldUseFallbackMap = not not useFallbackMap
		local exportAllProperties = EXPORT_ALL_PROPERTIES
		local getFallbackMap = Config.getClassPropertyFallbackMap
		local internBatchString = Config.internBatchString

		return function(
			state: ServiceState,
			instance: Instance,
			instanceIndex: number,
			forceSafeReads: boolean,
			strings: { string },
			stringIds: { [string]: number },
			compactOverlay: boolean?,
			includeDefaults: boolean?
		)
			local classValue = state.classValueByIndex[instanceIndex]
				or IdentityModule.compactClassValue(state, className)
			local parentIndex = state.parentIndexByIndex[instanceIndex]
			local attributes = if compactOverlay
				then false
				else serializeAttributesCompactV5(instance:GetAttributes(), state, strings, stringIds)
			local fallbackMap = if shouldUseFallbackMap then getFallbackMap(state, className) else nil
			local maskWords = nil
			local maskWordCount = 0
			local valuesOut = nil
			local valueWriteIndex = 0

			for i = 1, propertyCount do
				local propertyName = propertyNames[i]
				local value = nil
				local hasValue = false
				if
					not compactOverlay
					or not hotSchema.nativeRefReadIndices
					or not hotSchema.nativeRefReadIndices[i]
					or Config.nativeRefSelectionContains(hotSchema.nativeRefReadIndices[i], instanceIndex)
				then
					if forceSafeReads or (fallbackMap and fallbackMap[propertyName]) then
						local got, safeValue = tryRead(instance, propertyName)
						if got then
							value = safeValue
							hasValue = true
						end
					else
						value = (instance :: any)[propertyName]
						hasValue = true
					end
				end
				if
					propertyName == "Archivable"
					and state.originalNonArchivableInstances
					and state.originalNonArchivableInstances[instance]
				then
					value = false
					hasValue = true
				end
				if includeDefaults and not hasValue then
					error(`Failed to read {className}.{propertyName} during package preflight`)
				end
				if propertyName == "CustomPhysicalProperties" then
					hasValue, value =
						normalizeSchemaTransportValue(typeIds[i], propertyName, instance, hasValue, value)
				end
				if
					compactOverlay
					and hotSchema.nativeRefs
					and hotSchema.nativeRefs[i]
					and (not hotSchema.nativeRefReadIndices or not hotSchema.nativeRefReadIndices[i])
					and typeof(value) == "Instance"
					and value ~= state.instances[1]
					and value:IsDescendantOf(state.instances[1])
				then
					hasValue = false
				end

				if hasValue and value ~= nil then
					local isDefault = false
					if not exportAllProperties and not includeDefaults then
						local defaultComparable = defaults[i]
						local defaultFastComparable = fastDefaults[i]
						local compareMode = fastCompareModes[i]
						if compareMode == FAST_COMPARE_EQUAL then
							isDefault = value == defaultFastComparable
						elseif compareMode == FAST_COMPARE_VECTOR2 then
							isDefault = value.X == defaultFastComparable[1] and value.Y == defaultFastComparable[2]
						elseif compareMode == FAST_COMPARE_VECTOR3 then
							isDefault = value.X == defaultFastComparable[1]
								and value.Y == defaultFastComparable[2]
								and value.Z == defaultFastComparable[3]
						elseif compareMode == FAST_COMPARE_UDIM then
							isDefault = value.Scale == defaultFastComparable[1]
								and value.Offset == defaultFastComparable[2]
						elseif compareMode == FAST_COMPARE_UDIM2 then
							isDefault = value.X.Scale == defaultFastComparable[1]
								and value.X.Offset == defaultFastComparable[2]
								and value.Y.Scale == defaultFastComparable[3]
								and value.Y.Offset == defaultFastComparable[4]
						elseif compareMode == FAST_COMPARE_COLOR3 then
							isDefault = value.R == defaultFastComparable[1]
								and value.G == defaultFastComparable[2]
								and value.B == defaultFastComparable[3]
						elseif compareMode == FAST_COMPARE_BRICKCOLOR then
							isDefault = value.Number == defaultFastComparable
						elseif compareMode == FAST_COMPARE_ENUM_VALUE then
							isDefault = value.Value == defaultFastComparable
						elseif compareMode == FAST_COMPARE_CFRAME then
							local c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11 = value:GetComponents()
							isDefault = c0 == defaultFastComparable[1]
								and c1 == defaultFastComparable[2]
								and c2 == defaultFastComparable[3]
								and c3 == defaultFastComparable[4]
								and c4 == defaultFastComparable[5]
								and c5 == defaultFastComparable[6]
								and c6 == defaultFastComparable[7]
								and c7 == defaultFastComparable[8]
								and c8 == defaultFastComparable[9]
								and c9 == defaultFastComparable[10]
								and c10 == defaultFastComparable[11]
								and c11 == defaultFastComparable[12]
						elseif compareMode == FAST_COMPARE_RECT then
							isDefault = value.Min.X == defaultFastComparable[1]
								and value.Min.Y == defaultFastComparable[2]
								and value.Max.X == defaultFastComparable[3]
								and value.Max.Y == defaultFastComparable[4]
						else
							local compareFn = compareFns[i]
							if compareFn then
								isDefault = compareFn(value, defaultComparable, state)
							end
						end
					end
					if not isDefault then
						if skipEncode[i] then
							if includeDefaults then
								error(`Failed to encode {className}.{propertyName} during package preflight`)
							end
						else
							local encodeFn = encodeFns[i]
							local encoded = if encodeFn then encodeFn(value, state, strings, stringIds) else nil
							if encoded == nil then
								if includeDefaults then
									error(`Failed to encode {className}.{propertyName} during package preflight`)
								end
							else
								if maskWords == nil then
									maskWords = table.create(hotSchema.maxMaskWords)
									valuesOut = table.create(math.min(8, propertyCount))
								end
								local wordIndex = maskWordIndices[i]
								maskWords[wordIndex] = bit32.bor(maskWords[wordIndex] or 0, maskBitValues[i])
								if wordIndex > maskWordCount then
									maskWordCount = wordIndex
								end
								valueWriteIndex += 1
								valuesOut[valueWriteIndex] = encoded
							end
						end
					end
				end
			end

			local compactMask = compactPropertyMask(maskWords, maskWordCount)
			local compactValues = if valuesOut ~= nil and valueWriteIndex > 0 then valuesOut else false
			return buildCompactV5Row(
				state,
				instance,
				instanceIndex,
				classValue,
				parentIndex,
				attributes,
				compactMask,
				compactValues,
				compactOverlay,
				strings,
				stringIds,
				internBatchString
			)
		end
	end

	function Config.dynamicCompactTypeIdForValue(value: any): number?
		local valueType = typeof(value)
		if valueType == "boolean" then
			return COMPACT_TYPE_IDS.Bool
		elseif valueType == "number" then
			return COMPACT_TYPE_IDS.Number
		elseif valueType == "string" then
			return COMPACT_TYPE_IDS.String
		elseif valueType == "Vector2" then
			return COMPACT_TYPE_IDS.Vector2
		elseif valueType == "Vector3" then
			return COMPACT_TYPE_IDS.Vector3
		elseif valueType == "UDim" then
			return COMPACT_TYPE_IDS.UDim
		elseif valueType == "UDim2" then
			return COMPACT_TYPE_IDS.UDim2
		elseif valueType == "Color3" then
			return COMPACT_TYPE_IDS.Color3
		elseif valueType == "BrickColor" then
			return COMPACT_TYPE_IDS.BrickColor
		elseif valueType == "CFrame" then
			return COMPACT_TYPE_IDS.CFrame
		elseif valueType == "Rect" then
			return COMPACT_TYPE_IDS.Rect
		elseif valueType == "Font" then
			return COMPACT_TYPE_IDS.Font
		elseif valueType == "ColorSequence" then
			return COMPACT_TYPE_IDS.ColorSequence
		elseif valueType == "NumberSequence" then
			return COMPACT_TYPE_IDS.NumberSequence
		elseif valueType == "NumberRange" then
			return COMPACT_TYPE_IDS.NumberRange
		elseif valueType == "PhysicalProperties" then
			return COMPACT_TYPE_IDS.PhysicalProperties
		elseif valueType == "EnumItem" then
			return COMPACT_TYPE_IDS.EnumItem
		end
		return nil
	end

	serializeAttributesCompactV5 = function(
		attributes: { [string]: any },
		state: ServiceState,
		strings: { string },
		stringIds: { [string]: number }
	): any
		if type(attributes) ~= "table" or not next(attributes) then
			return false
		end

		local names = {}
		for name, _ in pairs(attributes) do
			names[#names + 1] = name
		end
		table.sort(names)

		local out = {}
		for _, name in ipairs(names) do
			local value = attributes[name]
			local typeId = Config.dynamicCompactTypeIdForValue(value)
			if typeId ~= nil then
				local encode = Config.encodeValueV5ByTypeId[typeId]
				local encoded = if typeId == COMPACT_TYPE_IDS.EnumItem and typeof(value) == "EnumItem"
					then {
						Config.internBatchString(strings, stringIds, tostring(value.EnumType)),
						Config.internBatchString(strings, stringIds, value.Name),
					}
					elseif encode then encode(value, state, strings, stringIds)
					else nil
				if encoded ~= nil then
					out[#out + 1] = Config.internBatchString(strings, stringIds, name)
					out[#out + 1] = typeId
					out[#out + 1] = encoded
				end
			end
		end

		if #out == 0 then
			return false
		end
		return out
	end

	function Config.exportCompactV5InstanceWithHotSchema(
		state: ServiceState,
		instance: Instance,
		instanceIndex: number,
		className: string,
		hotSchema: { [string]: any },
		forceSafeReads: boolean,
		strings: { string },
		stringIds: { [string]: number },
		compactOverlay: boolean?,
		includeDefaults: boolean?
	): any
		local useFallbackMap = forceSafeReads or hotSchema.usesFallbackMap
		local cacheKey = if useFallbackMap then "exporterWithFallback" else "exporter"
		local exporter = hotSchema[cacheKey]
		if not exporter then
			exporter = Config.buildCompactV5Exporter(className, hotSchema, useFallbackMap)
			hotSchema[cacheKey] = exporter
		end
		return exporter(
			state,
			instance,
			instanceIndex,
			forceSafeReads,
			strings,
			stringIds,
			compactOverlay,
			includeDefaults
		)
	end

	function Config.exportCompactV5InstanceIndexed(
		state: ServiceState,
		inst: Instance,
		instanceIndex: number,
		strings: { string },
		stringIds: { [string]: number },
		knownClassName: string?,
		knownHotSchema: any?,
		compactOverlay: boolean?,
		includeDefaults: boolean?
	)
		local className = knownClassName or state.classNameByIndex[instanceIndex] or inst.ClassName
		local hotSchema = knownHotSchema or Config.getHotPropertySchema(state, className)
		if not hotSchema.fallbacksLearned then
			Config.learnClassPropertyFallbacks(state, inst, className, hotSchema.names)
			hotSchema.usesFallbackMap = not not next(Config.getClassPropertyFallbackMap(state, className))
			hotSchema.fallbacksLearned = true
		end
		return Config.exportCompactV5InstanceWithHotSchema(
			state,
			inst,
			instanceIndex,
			className,
			hotSchema,
			false,
			strings,
			stringIds,
			compactOverlay,
			includeDefaults
		)
	end

	function Config.acquireDemandSerializerSlot()
		while activeDemandSerializers >= MAX_ACTIVE_DEMAND_SERIALIZERS do
			demandSerializerGate.Event:Wait()
		end
		activeDemandSerializers += 1
	end

	function Config.releaseDemandSerializerSlot()
		if activeDemandSerializers > 0 then
			activeDemandSerializers -= 1
		end
		demandSerializerGate:Fire()
	end

	prepareService = function(
		serviceName: string,
		nativeExportOnly: boolean?,
		nativeSnapshot: { [string]: any }?,
		nativeScriptSourcesOverride: { [Instance]: string }?,
		nativeInstancesOverride: { Instance }?
	): ({ [string]: any }?, ServiceState)
		if not ALLOWED_SERVICES[serviceName] then
			error("Unsupported service: " .. tostring(serviceName))
		end
		local service = game:GetService(serviceName)

		local nativeExport = nativeExportOnly == true
		local snapshotInstances = if nativeExport and nativeSnapshot then nativeSnapshot.instances else nil
		local providedInstances = snapshotInstances
		if providedInstances == nil and nativeExport and nativeInstancesOverride and nativeInstancesOverride[1] == service then
			providedInstances = nativeInstancesOverride
		end
		local descendants = providedInstances or service:GetDescendants()
		if not providedInstances then
			local excludedRoots = excludedExportRoots(serviceName, service)
			local descendantCount = #descendants
			local includedCount = 0
			if #excludedRoots == 0 then
				for index = 1, descendantCount do
					local instance = descendants[index]
					if instance.ClassName ~= "TouchTransmitter" then
						includedCount += 1
						descendants[includedCount] = instance
					end
				end
			else
				local excludedByInstance = {}
				for _, root in ipairs(excludedRoots) do
					excludedByInstance[root] = true
				end
				for index = 1, descendantCount do
					local instance = descendants[index]
					local excluded = excludedByInstance[instance] == true
						or excludedByInstance[instance.Parent] == true
					if excluded then
						excludedByInstance[instance] = true
					elseif instance.ClassName ~= "TouchTransmitter" then
						includedCount += 1
						descendants[includedCount] = instance
					end
				end
			end
			for index = includedCount + 1, descendantCount do
				descendants[index] = nil
			end
		end
		local expectedCount = if providedInstances then #providedInstances else #descendants + 1
		local instances = table.create(expectedCount)
		instances[1] = if providedInstances then providedInstances[1] else service
		local instanceCount = 1

		local scriptObjects = {}
		local scriptCount = 0
		local nativeLuaSourceIndices = if nativeExport then {} else nil
		local nativeNonArchivableIndices = if nativeExport then {} else nil
		local classNames = {}
		local classIdByName = {}
		local nameByIndex = if nativeExport then {} else table.create(expectedCount)
		local classNameByIndex = table.create(expectedCount)
		local classValueByIndex = table.create(expectedCount)
		local parentIndexByIndex = if nativeExport then {} else table.create(expectedCount)
		local unresolvedParentIndices = {}
		local serviceClassName = if nativeSnapshot then tostring(nativeSnapshot.serviceClassName) else service.ClassName
		local nativeScriptSources = if nativeSnapshot
			then nativeSnapshot.scriptSourcesByInstance
			else nativeScriptSourcesOverride
		local serviceIsLuaSourceContainer = not not Config.LUA_SOURCE_CLASS[serviceClassName]
		classNames[1] = serviceClassName
		classIdByName[serviceClassName] = 0
		local stateRoot = instances[1]
		local pathByInstance = if nativeSnapshot then nativeSnapshot.pathByInstance else { [stateRoot] = service.Name }
		local pathSegmentsByInstance = if nativeSnapshot
			then nativeSnapshot.pathSegmentsByInstance
			else { [stateRoot] = { service.Name } }
		local pathOrdinalsByInstance = if nativeSnapshot
			then nativeSnapshot.pathOrdinalsByInstance
			else { [stateRoot] = { 1 } }
		local debugIdByInstance: { [Instance]: string | boolean } = if nativeSnapshot
			then nativeSnapshot.debugIdByInstance
			else {}
		local instanceIdByInstance: { [Instance]: string | number | boolean } = {}
		local scriptKeyByInstance: { [Instance]: string } = {}
		local scriptSourcesByIndex = {}
		local nonArchivableInstances = {}
		local nonArchivableInstance
		local descendantsAreArchivable = nativeExport
			and not snapshotInstances
			and Config.studioChanges.hasNonArchivable(serviceName) == false
		if not snapshotInstances and not service.Archivable then
			nonArchivableInstance = service
			nonArchivableInstances[1] = service
			if nativeNonArchivableIndices then
				nativeNonArchivableIndices[1] = 1
			end
		end
		local nativeDebugIdData = if nativeSnapshot
			then nativeSnapshot.debugIdBuffer
			elseif nativeExport then table.create(expectedCount)
			else nil
		instanceIdByInstance[stateRoot] = 1
		nameByIndex[1] = service.Name
		classNameByIndex[1] = serviceClassName
		classValueByIndex[1] = 0
		parentIndexByIndex[1] = false
		if nativeDebugIdData and not nativeSnapshot then
			Config.writeNativeOverlayDebugId(service, nativeDebugIdData, 1)
		end

		if serviceIsLuaSourceContainer and not nativeExport then
			scriptCount += 1
			scriptObjects[scriptCount] = service
		end

		local firstDescendant = if providedInstances then 2 else 1
		for descendantIndex = firstDescendant, #descendants do
			local inst = descendants[descendantIndex]
			instanceCount += 1
			instances[instanceCount] = inst
			if not nativeExport then
				instanceIdByInstance[inst] = instanceCount
			end
			if nativeDebugIdData and not nativeSnapshot then
				Config.writeNativeOverlayDebugId(inst, nativeDebugIdData, instanceCount)
			end

			local className = inst.ClassName
			if classIdByName[className] == nil then
				classNames[#classNames + 1] = className
				classIdByName[className] = #classNames - 1
			end
			classNameByIndex[instanceCount] = className
			classValueByIndex[instanceCount] = classIdByName[className] or className
			if nativeExport then
				if not descendantsAreArchivable and not inst.Archivable then
					nonArchivableInstance = nonArchivableInstance or inst
					nonArchivableInstances[#nonArchivableInstances + 1] = inst
					nativeNonArchivableIndices[#nativeNonArchivableIndices + 1] = instanceCount
				end
				if Config.LUA_SOURCE_CLASS[className] then
					nativeLuaSourceIndices[#nativeLuaSourceIndices + 1] = instanceCount
					local source = if nativeScriptSources then nativeScriptSources[inst] else nil
					if source ~= nil then
						scriptCount += 1
						scriptObjects[scriptCount] = inst
						scriptSourcesByIndex[instanceCount] = source
					end
				end
			else
				local parent = inst.Parent
				nameByIndex[instanceCount] = inst.Name
				if parent ~= nil and parent ~= game then
					local resolvedParentIndex = instanceIdByInstance[parent]
					if resolvedParentIndex ~= nil then
						parentIndexByIndex[instanceCount] = resolvedParentIndex
					else
						unresolvedParentIndices[#unresolvedParentIndices + 1] = instanceCount
					end
				else
					parentIndexByIndex[instanceCount] = false
				end

				if Config.LUA_SOURCE_CLASS[className] then
					scriptCount += 1
					scriptObjects[scriptCount] = inst
				end
			end
		end
		if nativeDebugIdData and not nativeSnapshot and typeof(nativeDebugIdData) ~= "buffer" then
			nativeDebugIdData = buffer.fromstring(table.concat(nativeDebugIdData, "\0"))
		end

		local state: ServiceState = {
			instances = instances,
			nativeExportOnly = nativeExport,
			nativeSnapshotRoot = snapshotInstances ~= nil,
			nativeLiveSnapshot = if nativeSnapshot then nativeSnapshot.nativeLiveSnapshot == true else false,
			exportedInstances = if nativeSnapshot then nativeSnapshot.exportedInstances else nil,
			isExportedInstance = if nativeSnapshot then nativeSnapshot.isExportedInstance else nil,
			nativeDebugIds = if nativeSnapshot then nativeSnapshot.debugIds else nil,
			nonArchivableInstance = nonArchivableInstance,
			nonArchivableInstances = nonArchivableInstances,
			nativeDebugIdBuffer = nativeDebugIdData,
			nativeRootPropertyValues = if nativeSnapshot then nativeSnapshot.rootPropertyValues else nil,
			classNames = classNames,
			classIdByName = classIdByName,
			rootClassName = serviceClassName,
			pathByInstance = pathByInstance,
			pathSegmentsByInstance = pathSegmentsByInstance,
			pathOrdinalsByInstance = pathOrdinalsByInstance,
			debugIdByInstance = debugIdByInstance,
			instanceIdByInstance = instanceIdByInstance,
			nameByIndex = nameByIndex,
			classNameByIndex = classNameByIndex,
			classValueByIndex = classValueByIndex,
			parentIndexByIndex = parentIndexByIndex,
			scriptObjects = scriptObjects,
			scriptIndices = nil,
			scriptSources = {},
			scriptSourcesByIndex = scriptSourcesByIndex,
			nativeLuaSourceIndices = nativeLuaSourceIndices,
			nativeNonArchivableIndices = nativeNonArchivableIndices,
			nativeStructureGeneration = nil,
			matchedSettingsIds = nil,
			matchedSettingsIdVersion = nil,
			scriptInstancesByIndex = nil,
			scriptKeyByInstance = scriptKeyByInstance,
			batchCacheByKey = {},
			batchCacheKeys = {},
			sourceBatchCacheByKey = {},
			sourceBatchCacheKeys = {},
			servicePropertySchemaByClass = nil,
			hotPropertySchemaByClass = nil,
			requiresPcallByClassProperty = {},
		}
		refreshMatchedSettingsIds(state)

		if nativeExport then
			return nil, state
		end

		stateByService[serviceName] = state
		for _, index in ipairs(unresolvedParentIndices) do
			local parent = instances[index].Parent
			if parent ~= nil and parent ~= game then
				parentIndexByIndex[index] = instanceIdByInstance[parent] or false
			else
				parentIndexByIndex[index] = false
			end
		end
		for _, inst in ipairs(scriptObjects) do
			IdentityModule.getCachedScriptSourceKey(state, inst)
		end
		return {
			instanceCount = instanceCount,
			scriptCount = scriptCount,
			classNames = state.classNames,
			propertySchemaByClass = getServicePropertySchema(state),
			enumValueNamesByType = getServiceEnumValueNamesByType(state),
		},
			state
	end

	function Config.getBridgeInfo(): { [string]: any }
		local playerName, playerUserId = Config.getPlayerIdentity()
		local testArgs = if Config.startedInPlayMode
			then (game:GetService("StudioTestService") :: any):GetTestArgs()
			else nil
		local launch = if type(testArgs) == "table" and type(testArgs.__renium) == "table"
			then testArgs.__renium
			else nil
		return {
			runtimeId = Config.bridgeRuntimeId,
			launchNonce = if launch then launch.nonce else game:GetAttribute("__ReniumLaunchNonce"),
			launchEditRuntimeId = if launch then launch.editRuntimeId else game:GetAttribute("__ReniumEditRuntimeId"),
			playerName = playerName,
			playerUserId = playerUserId,
			placeId = game.PlaceId,
			gameId = game.GameId,
			placeName = game.Name,
			bridgeVersion = BRIDGE_VERSION,
			bridgeBuildUnix = BRIDGE_BUILD_UNIX,
			protocolVersion = BRIDGE_PROTOCOL_VERSION,
			codecVersion = CODEC_VERSION,
			bridgeRole = Config.bridgeRole,
			exportAllProperties = EXPORT_ALL_PROPERTIES,
		}
	end

	getState = function(serviceName: string): ServiceState
		local state = stateByService[serviceName]
		if not state then
			prepareService(serviceName)
			state = stateByService[serviceName]
		end
		if not state then
			error("State not prepared for service: " .. tostring(serviceName))
		end
		return state
	end

	function Config.boundedPositiveInteger(value: any, defaultValue: number, maximum: number): number
		local numeric = tonumber(value)
		if not numeric or numeric ~= numeric then
			return defaultValue
		end
		return math.clamp(math.floor(numeric), 1, maximum)
	end

	function Config.nativeRefSelectionContains(selection, instanceIndex: number): boolean
		if selection.packed == nil then
			return selection[instanceIndex]
		end
		local offset = selection.offset
		local candidate = selection.candidate
		local packed = selection.packed
		local length = buffer.len(packed)
		while candidate and candidate < instanceIndex do
			offset += 3
			if offset < length then
				candidate = buffer.readu8(packed, offset)
					+ bit32.lshift(buffer.readu8(packed, offset + 1), 8)
					+ bit32.lshift(buffer.readu8(packed, offset + 2), 16)
			else
				candidate = nil
			end
		end
		selection.offset = offset
		selection.candidate = candidate
		return candidate == instanceIndex
	end

	local function readPackedInstanceIndex(packed: buffer, offset: number): number
		return buffer.readu8(packed, offset)
			+ bit32.lshift(buffer.readu8(packed, offset + 1), 8)
			+ bit32.lshift(buffer.readu8(packed, offset + 2), 16)
	end

	local function decodeNativeRefSelection(rawSelection: { [any]: any }): any
		if type(rawSelection.packed) == "string" then
			local packed = EncodingService:Base64Decode(buffer.fromstring(rawSelection.packed))
			local count = tonumber(rawSelection.count)
			if not count or count < 1 or count % 1 ~= 0 or buffer.len(packed) ~= count * 3 then
				error("Invalid native reference selection")
			end
			local indices = table.create(count)
			for index = 1, count do
				indices[index] = readPackedInstanceIndex(packed, (index - 1) * 3)
			end
			return {
				packed = packed,
				offset = 0,
				candidate = indices[1],
				indices = indices,
			}
		end

		local selection = {}
		local selectedIndices = {}
		for _, rawIndex in ipairs(rawSelection) do
			local instanceIndex = tonumber(rawIndex)
			if instanceIndex and instanceIndex % 1 == 0 and instanceIndex > 0 and not selection[instanceIndex] then
				selection[instanceIndex] = true
				selectedIndices[#selectedIndices + 1] = instanceIndex
			end
		end
		if #selectedIndices == 0 then
			return nil
		end
		table.sort(selectedIndices)
		selection.indices = selectedIndices
		return selection
	end

	local function requestedNativeOverlayProperties(propertyNames: { any }?): ({ [string]: number }, { [string]: any })
		local requested = {}
		local requestedNativeRefIndices = {}
		if type(propertyNames) ~= "table" then
			return requested, requestedNativeRefIndices
		end
		for _, propertyEntry in ipairs(propertyNames) do
			if type(propertyEntry) == "string" then
				requested[propertyEntry] = 1
			elseif type(propertyEntry) == "table" and type(propertyEntry[1]) == "string" then
				local propertyName = propertyEntry[1]
				local selection = propertyEntry[2]
				if selection == true then
					requested[propertyName] = 2
				elseif type(selection) == "table" then
					local decodedSelection = decodeNativeRefSelection(selection)
					if decodedSelection then
						requested[propertyName] = 3
						requestedNativeRefIndices[propertyName] = decodedSelection
					end
				else
					requested[propertyName] = 1
				end
			end
		end
		return requested, requestedNativeRefIndices
	end

	function Config.getNativeOverlayHotSchema(
		state: ServiceState,
		className: string,
		propertyNames: { any }?
	): { [string]: any }
		local source = Config.getHotPropertySchema(state, className)
		local requested, requestedNativeRefIndices = requestedNativeOverlayProperties(propertyNames)
		local fields = {
			"typeIds",
			"enumTypes",
			"defaults",
			"fastDefaults",
			"fastCompareModes",
			"compareFns",
			"encodeFns",
			"skipEncode",
		}
		local out = {
			className = className,
			count = 0,
			maxMaskWords = 0,
			names = {},
			typeIds = {},
			enumTypes = {},
			defaults = {},
			fastDefaults = {},
			maskWordIndices = {},
			maskBitValues = {},
			fastCompareModes = {},
			compareFns = {},
			encodeFns = {},
			skipEncode = {},
			nativeRefs = {},
			nativeRefReadIndices = {},
			nativeRefCandidateIndices = {},
			nativeRefSelectionOnly = true,
			exporter = false,
			exporterWithFallback = false,
			fallbacksLearned = false,
			usesFallbackMap = false,
		}
		for sourceIndex, propertyName in ipairs(source.names) do
			local requestedMode = requested[propertyName]
			if requestedMode then
				local targetIndex = #out.names + 1
				out.names[targetIndex] = propertyName
				for _, field in ipairs(fields) do
					out[field][targetIndex] = source[field][sourceIndex]
				end
				out.maskWordIndices[targetIndex] = math.floor((targetIndex - 1) / 31) + 1
				out.maskBitValues[targetIndex] = bit32.lshift(1, (targetIndex - 1) % 31)
				out.nativeRefs[targetIndex] = requestedMode >= 2
				out.nativeRefReadIndices[targetIndex] = requestedNativeRefIndices[propertyName] or false
				if requestedMode == 3 then
					for _, instanceIndex in ipairs(requestedNativeRefIndices[propertyName].indices) do
						out.nativeRefCandidateIndices[instanceIndex] = true
					end
				else
					out.nativeRefSelectionOnly = false
				end
			end
		end
		local candidateIndices = {}
		for instanceIndex in pairs(out.nativeRefCandidateIndices) do
			candidateIndices[#candidateIndices + 1] = instanceIndex
		end
		table.sort(candidateIndices)
		out.nativeRefCandidateIndices = candidateIndices
		out.count = #out.names
		out.maxMaskWords = math.ceil(out.count / 31)
		return out
	end

	function Config.writeNativeOverlayDebugId(instance: Instance, values, offset: number)
		values[offset] = instance:GetDebugId(32)
	end

	function Config.finishNativeOverlayDebugIds(values): (buffer?, number)
		if values == nil then
			return nil, 0
		end
		if typeof(values) == "buffer" then
			return values, buffer.len(values)
		end
		local text = table.concat(values, "\0")
		return buffer.fromstring(text), #text
	end

	function Config.groupNativeOverlayItems(items, count: number): { any }
		local groups = {}
		local groupsByClass = {}
		for offset = 1, count do
			local item = items[offset]
			if item then
				local classValue = item[1]
				if classValue == nil then
					error("Native overlay item is missing its class")
				end
				local group = groupsByClass[classValue]
				if group == nil then
					group = { classValue, {} }
					groupsByClass[classValue] = group
					groups[#groups + 1] = group
				end
				item[1] = offset
				local rows = group[2]
				rows[#rows + 1] = item
			end
		end
		return groups
	end

	function Config.getCompactInstanceBatchVariantCacheKey(
		startIndex: number?,
		maxCount: number?,
		stableIdsEnabled: boolean?,
		overlayId: string?,
		overlayVariant: string?,
		overlayCacheKey: string?
	): string
		local key = ChunkingModule.getCompactInstanceBatchCacheKey(startIndex, maxCount)
		if stableIdsEnabled then
			key ..= ":stable-v1"
		end
		if type(overlayCacheKey) == "string" and overlayCacheKey ~= "" then
			key ..= ":overlay-cache:" .. overlayCacheKey
		elseif type(overlayId) == "string" and overlayId ~= "" then
			key ..= ":overlay:" .. overlayId
		end
		if type(overlayVariant) == "string" and overlayVariant ~= "" then
			key ..= ":variant:" .. overlayVariant
		end
		return key
	end

	function Config.getInstanceBatchCompact(
		serviceName: string,
		startIndex: number?,
		maxCount: number?,
		stableIdsEnabled: boolean?,
		overlayPropertiesByClass: { [string]: { any } }?,
		overlayId: string?,
		overlayVariant: string?,
		overlayCacheKey: string?,
		stateOverride: ServiceState?
	): (string, number)
		local state = stateOverride or getState(serviceName)
		local includeStableIds = not not stableIdsEnabled
		if refreshMatchedSettingsIds(state) then
			table.clear(state.batchCacheByKey)
			table.clear(state.batchCacheKeys)
		end
		local key = Config.getCompactInstanceBatchVariantCacheKey(
			startIndex,
			maxCount,
			includeStableIds,
			overlayId,
			overlayVariant,
			overlayCacheKey
		)
		local cachedPayload = state.batchCacheByKey[key]
		if cachedPayload then
			return cachedPayload, 0
		end

		local instances = state.instances
		local total = #instances
		local startPos = Config.boundedPositiveInteger(startIndex, 1, math.max(total + 1, 1))
		local take = Config.boundedPositiveInteger(maxCount, 300, math.max(total, 1))

		local function buildPayload(): { [string]: any }
			if startPos > total then
				return {
					format = BRIDGE_PROTOCOL_VERSION,
					codecVersion = CODEC_VERSION,
					total = total,
					strings = {},
					debugIds = if includeStableIds then {} else nil,
					settingsIds = nil,
					items = {},
				}
			end

			local finish = math.min(total, startPos + take - 1)
			local count = finish - startPos + 1
			local settingsIds = if includeStableIds
				then matchedSettingsIdsForRange(state, startPos, count)
				else nil
			local items = table.create(count)
			local strings = table.create(math.min(count * 2, 65536))
			local stringIds = {}
			local overlayHotSchemaByClass = nil
			local nativeRefCandidateSchemas = nil
			if type(overlayPropertiesByClass) == "table" then
				overlayHotSchemaByClass = {}
				local candidateSchemas = {}
				local candidateCount = 0
				local selectionOnly = true
				for _, className in ipairs(state.classNames) do
					local hotSchema =
						Config.getNativeOverlayHotSchema(state, className, overlayPropertiesByClass[className])
					overlayHotSchemaByClass[className] = hotSchema
					if hotSchema.count > 0 then
						if hotSchema.nativeRefSelectionOnly then
							candidateCount += #hotSchema.nativeRefCandidateIndices
							candidateSchemas[#candidateSchemas + 1] = {
								className = className,
								hotSchema = hotSchema,
							}
						else
							selectionOnly = false
						end
					end
				end
				if selectionOnly and candidateCount > 0 then
					nativeRefCandidateSchemas = candidateSchemas
				end
			end
			local snapshotDebugIds = if includeStableIds and overlayHotSchemaByClass then state.nativeDebugIds else nil
			local precomputedNativeDebugIds = snapshotDebugIds == nil
				and includeStableIds
				and overlayHotSchemaByClass
				and startPos == 1
				and count == total
				and state.nativeDebugIdBuffer ~= nil
			local nativeDebugIdData = if snapshotDebugIds
				then table.create(count)
				elseif precomputedNativeDebugIds then state.nativeDebugIdBuffer
				elseif includeStableIds and overlayHotSchemaByClass then table.create(count)
				else nil
			local debugIds = if includeStableIds and not nativeDebugIdData then table.create(count) else nil
			if snapshotDebugIds then
				for offset = 1, count do
					nativeDebugIdData[offset] = snapshotDebugIds[startPos + offset - 1] or ""
				end
			elseif nativeDebugIdData and not precomputedNativeDebugIds then
				for offset = 1, count do
					Config.writeNativeOverlayDebugId(instances[startPos + offset - 1], nativeDebugIdData, offset)
				end
			end
			if nativeRefCandidateSchemas then
				for _, candidateSchema in ipairs(nativeRefCandidateSchemas) do
					local className = candidateSchema.className
					local hotSchema = candidateSchema.hotSchema
					for _, i in ipairs(hotSchema.nativeRefCandidateIndices) do
						if state.nativeSnapshotRoot and i == 1 then
							continue
						end
						local offset = i - startPos + 1
						if offset >= 1 and offset <= count then
							local inst = instances[i]
							local actualClassName = state.classNameByIndex[i] or inst.ClassName
							if actualClassName ~= className then
								error("Native reference candidate class mismatch")
							end
							if debugIds then
								local debugId = IdentityModule.getCachedDebugId(state, inst)
								debugIds[offset] = if debugId
									then Config.internBatchString(strings, stringIds, debugId)
									else false
							end
							items[offset] = Config.exportCompactV5InstanceIndexed(
								state,
								inst,
								i,
								strings,
								stringIds,
								className,
								hotSchema,
								true,
								overlayVariant == "package-preflight-defaults"
							)
						end
					end
				end
			else
				ParallelModule.runParallelChunks(count, 1, function(startOffset, endOffset)
					local lastClassName = nil
					local lastHotSchema = nil
					for offset = startOffset, endOffset do
						local i = startPos + offset - 1
						if state.nativeSnapshotRoot and i == 1 then
							continue
						end
						local inst = instances[i]
						local className = state.classNameByIndex[i] or inst.ClassName
						local hotSchema = lastHotSchema
						if debugIds then
							local debugId = IdentityModule.getCachedDebugId(state, inst)
							debugIds[offset] = if debugId
								then Config.internBatchString(strings, stringIds, debugId)
								else false
						end
						if className ~= lastClassName then
							hotSchema = if overlayHotSchemaByClass
								then overlayHotSchemaByClass[className]
								else Config.getHotPropertySchema(state, className)
							lastClassName = className
							lastHotSchema = hotSchema
						end
						if not overlayHotSchemaByClass or hotSchema.count > 0 then
							items[offset] = Config.exportCompactV5InstanceIndexed(
								state,
								inst,
								i,
								strings,
								stringIds,
								className,
								hotSchema,
								overlayHotSchemaByClass ~= nil,
								overlayVariant == "package-preflight-defaults"
							)
						end
					end
				end)
			end
			if overlayHotSchemaByClass then
				local classGroups = Config.groupNativeOverlayItems(items, count)
				local debugIdBuffer, debugIdBufferBytes = Config.finishNativeOverlayDebugIds(nativeDebugIdData)
				return {
					format = "native-overlay-v3",
					codecVersion = CODEC_VERSION,
					total = total,
					strings = strings,
					debugIdBuffer = debugIdBuffer,
					debugIdEncoding = if debugIdBuffer then "nul-text-v1" else nil,
					debugIdBufferBytes = debugIdBufferBytes,
					settingsIds = settingsIds,
					items = classGroups,
				}
			end
			return {
				format = BRIDGE_PROTOCOL_VERSION,
				codecVersion = CODEC_VERSION,
				total = total,
				strings = strings,
				debugIds = debugIds,
				settingsIds = settingsIds,
				items = items,
			}
		end

		local acquired = false
		local ok, payload = xpcall(function()
			Config.acquireDemandSerializerSlot()
			acquired = true
			return buildPayload()
		end, debug.traceback)
		if acquired then
			Config.releaseDemandSerializerSlot()
		end
		if not ok then
			error(payload, 0)
		end
		local encoded, encodeMs = ChunkingModule.jsonEncodeTimed(payload)

		Config.cacheBatchPayload(state.batchCacheByKey, state.batchCacheKeys, key, encoded, 256)
		return encoded, encodeMs
	end

	function Config.cacheBatchPayload(
		cacheByKey: { [string]: string },
		cacheKeys: { string },
		key: string,
		payload: string,
		limit: number
	)
		cacheByKey[key] = payload
		cacheKeys[#cacheKeys + 1] = key
		if #cacheKeys > limit then
			local oldestKey = table.remove(cacheKeys, 1)
			if oldestKey and oldestKey ~= key then
				cacheByKey[oldestKey] = nil
			end
		end
	end

	function Config.readScriptSource(scriptInstance: Instance?, description: string): string
		if not scriptInstance then
			error(description .. " no longer exists in the export snapshot")
		end
		local ok, source =
			pcall(ScriptEditorService.GetEditorSource, ScriptEditorService, scriptInstance :: LuaSourceContainer)
		if not ok then
			ok, source = pcall(function()
				return scriptInstance.Source
			end)
		end
		if not ok then
			error(`Unable to read {description}: {source}`)
		end
		if type(source) ~= "string" then
			error(description .. " returned a non-string Source value")
		end
		return source
	end

	function Config.getSourceForIndex(state: ServiceState, sourceIndex: number): string
		local src = state.scriptSourcesByIndex[sourceIndex]
		if src then
			return src
		end

		local scriptInstance = state.scriptInstancesByIndex and state.scriptInstancesByIndex[sourceIndex] or nil
		src = Config.readScriptSource(scriptInstance, "script source index " .. tostring(sourceIndex))
		state.scriptSourcesByIndex[sourceIndex] = src
		return src
	end

	function Config.getSourceRangeBatchCompact(
		serviceName: string,
		startIndex: number?,
		maxCount: number?,
		exportId: string?
	): (string, number)
		local state = if exportId and exportId ~= ""
			then editorSync.getBinaryExportState(exportId, serviceName)
			else getState(serviceName)
		ensureScriptRangeIndex(state)
		local total = state.scriptIndices and #state.scriptIndices or 0
		local startPos = Config.boundedPositiveInteger(startIndex, 1, math.max(total + 1, 1))
		local take = Config.boundedPositiveInteger(maxCount, 64, MAX_SOURCE_BATCH_PATHS)
		local cacheKey = ChunkingModule.getSourceRangeBatchCacheKey(startPos, take)
		local cachedPayload = state.sourceBatchCacheByKey[cacheKey]
		if cachedPayload then
			return cachedPayload, 0
		end

		local encoded: string
		local encodeMs = 0
		if startPos > total then
			encoded, encodeMs = ChunkingModule.jsonEncodeTimed({
				items = {},
			})
		else
			local finish = math.min(total, startPos + take - 1)
			local count = finish - startPos + 1
			local indicesByIndex = table.create(count)
			local sourcesByIndex = table.create(count)
			local workerCount = ParallelModule.getParallelChunkWorkerCount(count, PARALLEL_SOURCE_BATCH_MIN_ITEMS)
			ParallelModule.runParallelChunks(count, workerCount, function(startOffset, endOffset)
				for offset = startOffset, endOffset do
					local scriptIndex = startPos + offset - 1
					local sourceIndex = state.scriptIndices and state.scriptIndices[scriptIndex] or 0
					indicesByIndex[offset] = sourceIndex
					sourcesByIndex[offset] = Config.getSourceForIndex(state, sourceIndex)
				end
			end)

			local items = table.create(count * 2)
			for offset = 1, count do
				items[#items + 1] = indicesByIndex[offset]
				items[#items + 1] = sourcesByIndex[offset]
			end
			encoded, encodeMs = ChunkingModule.jsonEncodeTimed({
				items = items,
			})
		end

		Config.cacheBatchPayload(state.sourceBatchCacheByKey, state.sourceBatchCacheKeys, cacheKey, encoded, 64)
		return encoded, encodeMs
	end

	local function hashBatchPayload(encoded: string): (string, number)
		local started = os.clock()
		local hash = buffer.tostring(
			EncodingService:Base64Encode(
				EncodingService:ComputeBufferHash(buffer.fromstring(encoded), Enum.HashAlgorithm.Blake3)
			)
		)
		return hash, (os.clock() - started) * 1000
	end

	local function tryCompressBatchPayload(encoded: string, maxLen: number, encodeMs: number): { [string]: any }?
		local started = os.clock()
		local ok, compressedText = pcall(function()
			local compressed = EncodingService:CompressBuffer(
				buffer.fromstring(encoded),
				Enum.CompressionAlgorithm.Zstd,
				1
			)
			return buffer.tostring(EncodingService:Base64Encode(compressed))
		end)
		local compressionMs = (os.clock() - started) * 1000
		if not ok or type(compressedText) ~= "string" or #compressedText >= #encoded or #compressedText > maxLen then
			return nil
		end
		return {
			start = 1,
			nextStart = #compressedText + 1,
			total = #compressedText,
			chunk = compressedText,
			pluginEncodeMs = encodeMs + compressionMs,
			compression = "zstd-base64-v1",
			uncompressedBytes = #encoded,
		}
	end

	local function removeBatchCacheEntry(state: ServiceState, cacheKey: string)
		state.batchCacheByKey[cacheKey] = nil
		for index, cachedKey in ipairs(state.batchCacheKeys) do
			if cachedKey == cacheKey then
				table.remove(state.batchCacheKeys, index)
				return
			end
		end
	end

	local function buildInstanceBatchPayload(
		encoded: string,
		encodeMs: number,
		chunkStart: number?,
		maxLen: number?,
		compressedPayloadsEnabled: boolean?,
		knownPayloadHash: string?,
		payloadCacheEnabled: boolean?
	): { [string]: any }
		local result
		local payloadHash
		if payloadCacheEnabled == true then
			local hashMs
			payloadHash, hashMs = hashBatchPayload(encoded)
			encodeMs += hashMs
			if payloadHash == knownPayloadHash then
				result = {
					start = 1,
					nextStart = 1,
					total = #encoded,
					chunk = "",
					pluginEncodeMs = encodeMs,
					payloadHash = payloadHash,
					payloadCacheHit = true,
				}
			end
		end

		local requestedStart = math.max(1, math.floor(tonumber(chunkStart) or 1))
		local requestedLen = math.clamp(math.floor(tonumber(maxLen) or 2000), 1, 8 * 1024 * 1024)
		if result == nil and compressedPayloadsEnabled == true and requestedStart == 1 then
			result = tryCompressBatchPayload(encoded, requestedLen, encodeMs)
		end
		if result == nil then
			result = ChunkingModule.chunkEncodedString(encoded, chunkStart, maxLen, encodeMs)
		end
		result.payloadHash = payloadHash
		result.payloadCacheHit = result.payloadCacheHit == true
		return result
	end

	function Config.getInstanceBatchCompactChunk(
		serviceName: string,
		startIndex: number?,
		maxCount: number?,
		chunkStart: number?,
		maxLen: number?,
		stableIdsEnabled: boolean?,
		overlayPropertiesByClass: { [string]: { any } }?,
		overlayId: string?,
		overlayVariant: string?,
		stateOverride: ServiceState?,
		compressedPayloadsEnabled: boolean?,
		knownPayloadHash: string?,
		payloadCacheEnabled: boolean?,
		overlayCacheKey: string?
	): { [string]: any }
		local encoded, encodeMs = Config.getInstanceBatchCompact(
			serviceName,
			startIndex,
			maxCount,
			not not stableIdsEnabled,
			overlayPropertiesByClass,
			overlayId,
			overlayVariant,
			overlayCacheKey,
			stateOverride
		)
		local result = buildInstanceBatchPayload(
			encoded,
			math.max(0, encodeMs or 0),
			chunkStart,
			maxLen,
			compressedPayloadsEnabled,
			knownPayloadHash,
			payloadCacheEnabled
		)
		if
			type(overlayId) == "string"
			and overlayId ~= ""
			and not (type(overlayCacheKey) == "string" and overlayCacheKey ~= "")
			and (result.payloadCacheHit or result.nextStart > result.total)
		then
			local state = stateOverride or getState(serviceName)
			local key = Config.getCompactInstanceBatchVariantCacheKey(
				startIndex,
				maxCount,
				not not stableIdsEnabled,
				overlayId,
				overlayVariant,
				overlayCacheKey
			)
			removeBatchCacheEntry(state, key)
		end
		return result
	end

	function Config.getSourceRangeBatchCompactChunk(
		serviceName: string,
		startIndex: number?,
		maxCount: number?,
		chunkStart: number?,
		maxLen: number?,
		exportId: string?
	): { [string]: any }
		local encoded, encodeMs = Config.getSourceRangeBatchCompact(serviceName, startIndex, maxCount, exportId)
		return ChunkingModule.chunkEncodedString(encoded, chunkStart, maxLen, encodeMs)
	end

	Config.bridgeMethodHandlers = {}

	Config.bridgeMethodHandlers.getBridgeInfo = function()
		return Config.getBridgeInfo()
	end
	local function cancelRequestLeaseResources(leaseId: string)
		Config.studioChanges.cancelWait(leaseId)
		local editorResult = editorSync.cancelRequestLease(leaseId)
		local cancelledUploads = Config.editorTransactionUploads.cancelLease(leaseId)
		local creatorResult = Config.creatorApi.cancelRequestLease(leaseId)
		Config.afterBridgeExclusiveIdle(function()
			local result = editorSync.finishRequestLeaseCancellation(leaseId)
			for _, transactionId in ipairs(result.transactionIds) do
				transactionExpectations[transactionId] = nil
			end
		end)
		return editorResult, cancelledUploads, creatorResult
	end
	Config.bridgeMethodHandlers.cancelRequestLease = function(p)
		local leaseId = p.leaseId
		if type(leaseId) ~= "string" or leaseId == "" or #leaseId > 128 then
			error("Invalid request lease id")
		end
		local bridgeResult = Config.cancelBridgeRequestLease(leaseId)
		local editorResult, cancelledUploads, creatorResult = cancelRequestLeaseResources(leaseId)
		return {
			ok = true,
			active = bridgeResult.active or editorResult.active,
			queued = bridgeResult.queued,
			uploads = cancelledUploads,
			creatorJobs = creatorResult.jobs,
			cameras = creatorResult.cameras,
		}
	end
	Config.bridgeMethodHandlers.setUpdateStatus = function(p)
		local version = p.latestVersion
		local available = not Config.startedInPlayMode
			and plugin:GetSetting(SETTINGS_PREFIX .. "notifications") ~= false
			and UpdateModule.isNewer(version, BRIDGE_VERSION)
		if available then
			ui.notify(
				"update-" .. version,
				`Renium {version} is available`,
				"Update the editor extension and Studio plugin together.",
				"Update",
				function()
					queueEditorAction({ type = "installUpdate", version = version })
				end,
				true
			)
		end
		return { ok = true, available = available }
	end

	Config.bridgeMethodHandlers.configurePropertyCandidates = function(p)
		return configurePropertyCandidates(p.classes)
	end

	Config.bridgeMethodHandlers.setExportOptions = configureExportOptions

	Config.bridgeMethodHandlers.beginEditorPushReview = function(p)
		pruneEditorReviewUploads()
		local uploadId = tostring(p.uploadId or "")
		local totalChunks = tonumber(p.totalChunks)
		local changeCount = tonumber(p.changeCount)
		local rowCount = tonumber(p.rowCount)
		if uploadId == "" or not totalChunks or totalChunks < 1 or totalChunks > 4096 or totalChunks % 1 ~= 0 then
			error("Invalid editor review upload")
		end
		if not changeCount or changeCount < 0 or changeCount > MAX_EDITOR_REVIEW_CHANGES or changeCount % 1 ~= 0 then
			error("Invalid editor review change count")
		end
		if not rowCount or rowCount < 1 or rowCount > MAX_EDITOR_REVIEW_CHANGES or rowCount % 1 ~= 0 then
			error("Invalid editor review row count")
		end
		if not Config.editorReviewUploads[uploadId] and editorReviewUploadCount() >= MAX_EDITOR_REVIEW_UPLOADS then
			error("Too many active editor review uploads")
		end
		Config.editorReviewUploads[uploadId] = {
			changeCount = changeCount,
			rowCount = rowCount,
			totalChunks = totalChunks,
			chunks = table.create(totalChunks),
			receivedChunks = 0,
			receivedRows = 0,
			updatedAt = os.clock(),
		}
		armEditorReviewUploadExpiry(uploadId, Config.editorReviewUploads[uploadId])
		return { ok = true, uploadId = uploadId }
	end

	Config.bridgeMethodHandlers.appendEditorPushReview = function(p)
		pruneEditorReviewUploads()
		local uploadId = tostring(p.uploadId or "")
		local upload = Config.editorReviewUploads[uploadId]
		if type(upload) ~= "table" then
			error("Editor review upload was not found")
		end
		local index = tonumber(p.index)
		if not index or index < 1 or index > upload.totalChunks or index % 1 ~= 0 or type(p.rows) ~= "table" then
			error("Invalid editor review upload chunk")
		end
		if not upload.chunks[index] then
			if upload.receivedRows + #p.rows > upload.rowCount then
				error("Editor review upload exceeds its declared row count")
			end
			upload.chunks[index] = p.rows
			upload.receivedChunks += 1
			upload.receivedRows += #p.rows
		end
		armEditorReviewUploadExpiry(uploadId, upload)
		return { ok = true, rows = #p.rows }
	end

	Config.bridgeMethodHandlers.finishEditorPushReview = function(p)
		pruneEditorReviewUploads()
		local uploadId = tostring(p.uploadId or "")
		local upload = Config.editorReviewUploads[uploadId]
		Config.editorReviewUploads[uploadId] = nil
		if
			type(upload) ~= "table"
			or upload.receivedChunks ~= upload.totalChunks
			or upload.receivedRows ~= upload.rowCount
		then
			error("Editor review upload is incomplete")
		end
		local rows = {}
		for index = 1, upload.totalChunks do
			for _, row in ipairs(upload.chunks[index]) do
				rows[#rows + 1] = row
			end
		end
		return ui.requestEditorPushReview(
			{
				changeCount = upload.changeCount,
				rows = rows,
			},
			Config.getBridgeSettings(),
			{
				decodeValue = editorSync.decodeReviewValue,
				readProperty = editorSync.readReviewProperty,
				valuesEqual = EditorSyncModule.valuesEqual,
				resolveInstance = editorSync.resolveReviewInstance,
			}
		)
	end

	Config.bridgeMethodHandlers.cancelEditorPushReview = function(p)
		local uploadId = tostring(p.uploadId or "")
		local found = Config.editorReviewUploads[uploadId] ~= nil
		Config.editorReviewUploads[uploadId] = nil
		return { ok = true, found = found }
	end

	Config.bridgeMethodHandlers.requestEditorPushReview = function(p)
		return ui.requestEditorPushReview(p, Config.getBridgeSettings(), {
			decodeValue = editorSync.decodeReviewValue,
			readProperty = editorSync.readReviewProperty,
			valuesEqual = EditorSyncModule.valuesEqual,
			resolveInstance = editorSync.resolveReviewInstance,
		})
	end

	Config.bridgeMethodHandlers.requestProtectedWriteReview = function(p)
		return ui.requestProtectedWriteReview(p, {
			decodeValue = editorSync.decodeReviewValue,
			readProperty = editorSync.readReviewProperty,
			valuesEqual = EditorSyncModule.valuesEqual,
			resolveInstance = editorSync.resolveReviewInstance,
		})
	end

	Config.bridgeMethodHandlers.getEditorPushReviewDecision = ui.getEditorPushReviewDecision
	Config.bridgeMethodHandlers.setEditorPushReviewDecision = ui.setEditorPushReviewDecision

	Config.bridgeMethodHandlers.beginEditorBinaryExport = function(p)
		return editorSync.beginBinaryExport(p)
	end
	Config.bridgeMethodHandlers.awaitEditorBinaryExport = editorSync.awaitBinaryExport
	Config.bridgeMethodHandlers.readEditorBinaryExport = editorSync.readBinaryExport
	Config.bridgeMethodHandlers.readEditorBinaryExportBatch = editorSync.readBinaryExportBatch
	Config.bridgeMethodHandlers.finishEditorBinaryExport = function(p)
		local result = editorSync.finishBinaryExport(p)
		if p.recordSyncCompletion == true then
			Config.recordSyncCompletion()
			result.syncCompletionRecorded = true
		end
		return result
	end
	Config.bridgeMethodHandlers.beginEditorBinaryImport = editorSync.beginBinaryImport
	Config.bridgeMethodHandlers.appendEditorBinaryImport = editorSync.appendBinaryImport
	Config.bridgeMethodHandlers.cancelEditorBinaryImport = editorSync.cancelBinaryImport
	Config.bridgeMethodHandlers.cancelEditorReconcile = editorSync.cancelReconcile
	Config.bridgeMethodHandlers.getEditorFilterCandidates = editorSync.getFilterCandidates
	Config.bridgeMethodHandlers.getEditorMutationPackages = editorSync.getMutationPackages
	Config.bridgeMethodHandlers.getEditorServiceChangeGenerations = editorSync.getServiceChangeGenerations
	Config.editorTransactionUploads = TransactionUploadModule.create(
		editorSync.beginTransaction,
		function(id, params)
			beginEditorTransactionExpectation(id, params)
		end,
		ValueEqualityModule.exactValuesEqual
	)
	Config.bridgeMethodHandlers.beginEditorTransactionUpload = function(p, _, leaseId)
		return Config.editorTransactionUploads.begin(p, leaseId)
	end
	Config.bridgeMethodHandlers.appendEditorTransactionUpload = function(p, _, leaseId)
		return Config.editorTransactionUploads.append(p, leaseId)
	end
	Config.bridgeMethodHandlers.finishEditorTransactionUpload = function(p, _, leaseId)
		return Config.editorTransactionUploads.finish(p, leaseId)
	end
	Config.bridgeMethodHandlers.cancelEditorTransactionUpload = function(p, _, leaseId)
		return Config.editorTransactionUploads.cancel(p, leaseId)
	end
	Config.bridgeMethodHandlers.beginEditorTransaction = function(p)
		local result = editorSync.beginTransaction(p)
		if result.ok == true then
			beginEditorTransactionExpectation(tostring(p.transactionId or ""), p)
		end
		return result
	end
	Config.bridgeMethodHandlers.commitEditorTransaction = function(p)
		if transactionExpectations[tostring(p.transactionId or "")] ~= nil then
			return editorSync.commitTransaction(p)
		end
		return BridgePluginRuntime.withSuppression(Config.studioChanges, editorSync.commitTransaction, p)
	end
	Config.bridgeMethodHandlers.rollbackEditorTransaction = function(p)
		if transactionExpectations[tostring(p.transactionId or "")] ~= nil then
			return editorSync.rollbackTransaction(p)
		end
		return BridgePluginRuntime.withSuppression(Config.studioChanges, editorSync.rollbackTransaction, p)
	end
	Config.bridgeMethodHandlers.getEditorTransactionState = function(p)
		return editorSync.getTransactionState(p)
	end

	Config.bridgeMethodHandlers.finishEditorBinaryImport = function(p)
		return BridgePluginRuntime.withSuppression(Config.studioChanges, editorSync.finishBinaryImport, p)
	end

	Config.bridgeMethodHandlers.applyEditorChanges = function(p)
		local transactionScoped = transactionExpectations[tostring(p.transactionId or "")] ~= nil
		if transactionScoped then
			return editorSync.applyChanges(p)
		end
		return BridgePluginRuntime.withSuppression(Config.studioChanges, editorSync.applyChanges, p, p)
	end

	Config.bridgeMethodHandlers.sampleStudioProperty = function(p)
		assert(type(p.property) == "string" and p.property ~= "", "Invalid property sample")
		local instance = IdentityModule.resolvePathSegments(p.pathSegments, nil, p.pathOrdinals)
		assert(instance ~= nil and instance.ClassName == p.className, "Property sample target changed")
		Config.studioChanges.samplePropertyChange(instance, p.property)
		return { ok = true }
	end

	Config.bridgeMethodHandlers.getStudioChangeState = function(p, _sessionGeneration, leaseId)
		if type(p.liveSyncStatus) == "table" then
			editorSyncStats.liveSync = p.liveSyncStatus
			Config.updateStatusText()
		end
		local runtimeSettings = Config.getBridgeSettings()
		if tostring(p.runtimeId or "") == Config.bridgeRuntimeId then
			Config.ackPendingBridgeSettingChanges(p.ackRuntimeSettingsSeq)
		end
		local runtimeSettingChanges, runtimeSettingsSeq, runtimeSettingChangeCount =
			Config.getPendingBridgeSettingChanges()
		local pendingActions = pendingEditorActions(p.ackEditorActions, p.runtimeId)
		local compact = p.compact == true
		if runtimeSettings.twoWaySync == false then
			-- Manual pushes still need transaction guards and acknowledgements when
			-- user-facing two-way sync is disabled. Do not silently turn an ordinary
			-- status request into persistent tracking, but preserve every internal
			-- guard/lease/ack parameter.
			local internalParams = table.clone(p)
			if type(p.trackingGuardId) ~= "string" or p.trackingGuardId == "" then
				internalParams.start = false
			end
			internalParams.compact = true
			local guardState = Config.studioChanges.getState(internalParams, leaseId)
			return {
				ok = true,
				tracking = false,
				role = Config.bridgeRole,
				dirtyServices = {},
				fullSyncServices = {},
				propertyChanges = {},
				changes = {},
				propertyChangeCount = 0,
				changeCount = 0,
				twoWaySyncEnabled = false,
				runtimeSettingChanges = runtimeSettingChanges,
				runtimeSettingChangeCount = runtimeSettingChangeCount,
				runtimeSettingsSeq = runtimeSettingsSeq,
				runtimeId = Config.bridgeRuntimeId,
				seq = guardState.seq,
				snapshotSeq = guardState.snapshotSeq,
				serviceGenerations = guardState.serviceGenerations,
				pendingEpoch = guardState.pendingEpoch,
				restoredPendingEpoch = guardState.restoredPendingEpoch,
				editorActions = if compact then {} else pendingActions,
				editorActionCount = #pendingActions,
				operation = editorSync.operationState(),
			}
		end
		local changeState = Config.studioChanges.getState(p, leaseId)
		changeState.twoWaySyncEnabled = true
		changeState.runtimeSettingChanges = runtimeSettingChanges
		changeState.runtimeSettingChangeCount = runtimeSettingChangeCount
		changeState.runtimeSettingsSeq = runtimeSettingsSeq
		changeState.editorActions = if compact then {} else pendingActions
		changeState.editorActionCount = #pendingActions
		changeState.operation = editorSync.operationState()
		return changeState
	end

	Config.bridgeMethodHandlers.getConsoleOutput = RuntimeApi.getConsoleOutput
	Config.bridgeMethodHandlers.getGuiBounds = RuntimeApi.getGuiBounds
	Config.bridgeMethodHandlers.getGuiInventory = RuntimeApi.getGuiInventory
	Config.bridgeMethodHandlers.getWorldPoint = RuntimeApi.getWorldPoint
	Config.bridgeMethodHandlers.getMouseLocation = RuntimeApi.getMouseLocation
	Config.bridgeMethodHandlers.sendVirtualInput = RuntimeApi.sendVirtualInput
	Config.bridgeMethodHandlers.deviceSimulator = RuntimeApi.deviceSimulator
	Config.bridgeMethodHandlers.networkSimulation = function(p)
		local isClient = Config.bridgeRole == "play-client"
		assert((p.client == true) == isClient, "Network target changed; select the current client")
		if isClient then
			assert(not RunService:IsEdit() and game:GetService("Players").LocalPlayer ~= nil, "The selected play client has stopped")
		elseif p.action ~= "show" then
			assert(RunService:IsEdit(), "Use --player to change networking during Play")
		end
		local result = networkSimulation.handle(p)
		result.runtimeId = Config.bridgeRuntimeId
		result.scope = if isClient then "client-process" else "studio-settings"
		return result
	end
	Config.bridgeMethodHandlers.performance = function(p)
		assert(p.runtimeId == Config.bridgeRuntimeId, "Performance target changed; select the current runtime")
		if p.action == "micro-start" then
			microProfiler.start(p.frames or 256)
			return { ok = true, runtimeId = Config.bridgeRuntimeId, state = "collecting", frameLimit = p.frames or 256, scope = "studio-process" }
		elseif p.action == "micro-stop" then
			return microProfiler.stop(function()
				return performance.micro(p)
			end)
		elseif p.action == "micro-read" then
			return performance.microRead(p)
		elseif p.action == "micro" then
			return microProfiler.read(function()
				return performance.micro(p)
			end)
		end
		assert(p.action == "snapshot" or p.action == "start" or p.action == "stop" or p.action == "read", "Unknown performance action")
		return performance[p.action](p)
	end
	Config.bridgeMethodHandlers.captureViewportProbe = RuntimeApi.captureViewportProbe
	Config.bridgeMethodHandlers.executeLuau = RuntimeApi.executeLuau
	Config.bridgeMethodHandlers.startStopPlay = RuntimeApi.startStopPlay
	Config.bridgeMethodHandlers.getStudioState = RuntimeApi.studioState
	Config.bridgeMethodHandlers.getCreatorContext = Config.creatorApi.creatorContext
	Config.bridgeMethodHandlers.cameraCapture = Config.creatorApi.cameraCapture
	Config.bridgeMethodHandlers.insertAsset = Config.creatorApi.insertAsset
	Config.bridgeMethodHandlers.generateModel = Config.creatorApi.generateModel
	Config.bridgeMethodHandlers.creatorJob = Config.creatorApi.creatorJob
	Config.bridgeMethodHandlers.multiEdit = Config.creatorApi.multiEdit
	Config.bridgeMethodHandlers.uploadImages = Config.creatorApi.uploadImages

	Config.bridgeMethodHandlers.recordSyncCompletion = function()
		Config.recordSyncCompletion()
		return { ok = true }
	end

	Config.bridgeMethodHandlers.getEditorBinaryOverlayChunk = function(p)
		if type(p.overlayPropertiesByClass) ~= "table" then
			error("Native export overlay properties must be an object")
		end
		local overlayId = tostring(p.overlayId or "")
		if overlayId == "" or #overlayId > 128 then
			error("Invalid native export overlay id")
		end
		local serviceName = tostring(p.service)
		local overlayCacheKey = tostring(p.overlayCacheKey or "")
		if #overlayCacheKey > 128 then
			error("Invalid native export overlay cache key")
		end
		local state = editorSync.getBinaryExportState(overlayId, serviceName)
		local result = Config.getInstanceBatchCompactChunk(
			serviceName,
			p.startIndex,
			p.maxCount,
			p.chunkStart,
			p.maxLen,
			p.supportsStableInstanceIds ~= false,
			p.overlayPropertiesByClass,
			overlayId,
			tostring(p.overlayVariant or ""),
			state,
			p.supportsCompressedPayload == true,
			tostring(p.knownPayloadHash or ""),
			p.supportsPayloadCache == true,
			overlayCacheKey
		)
		editorSync.validateBinaryExportState(overlayId, serviceName)
		return result
	end

	Config.bridgeMethodHandlers.getSourceRangeBatchCompactChunk = function(p)
		return Config.getSourceRangeBatchCompactChunk(
			tostring(p.service),
			p.startIndex,
			p.maxCount,
			p.chunkStart,
			p.maxLen,
			tostring(p.exportId or "")
		)
	end

	Config.bridgeMethodHandlers.getLiveSourceBatch = function(p)
		return editorSync.getLiveSourceBatch(p)
	end

	function Config.handleMethod(
		method: string,
		params: { [string]: any },
		sessionGeneration: number?,
		leaseId: string?
	): any
		local handler = Config.bridgeMethodHandlers[method]
		if not handler then
			error("Unknown method: " .. tostring(method))
		end
		local allowCancelled = method == "cancelRequestLease"
			or method == "getEditorTransactionState"
			or method == "rollbackEditorTransaction"
			or method == "cancelEditorTransactionUpload"
			or method == "cancelEditorBinaryImport"
			or method == "cancelEditorReconcile"
			or method == "cancelEditorPushReview"
			or method == "finishEditorBinaryExport"
		return editorSync.withRequestLease(
			leaseId,
			allowCancelled,
			handler,
			params,
			sessionGeneration,
			leaseId
		)
	end

	Config.bridgeExclusiveMethods = {
		configurePropertyCandidates = true,
		setExportOptions = true,
		applyEditorChanges = true,
		beginEditorTransaction = true,
		beginEditorTransactionUpload = true,
		finishEditorTransactionUpload = true,
		cancelEditorTransactionUpload = true,
		commitEditorTransaction = true,
		rollbackEditorTransaction = true,
		beginEditorBinaryImport = true,
		appendEditorBinaryImport = true,
		finishEditorBinaryImport = true,
		beginEditorBinaryExport = true,
		finishEditorBinaryExport = true,
		deviceSimulator = true,
		networkSimulation = true,
		captureViewportProbe = true,
		sendVirtualInput = true,
		executeLuau = true,
		startStopPlay = true,
		cameraCapture = true,
		insertAsset = true,
		generateModel = true,
		multiEdit = true,
		uploadImages = true,
	}
	Config.bridgeSessionOwnedMethods = {
		-- Profiling is synchronous and independent of place mutations. Keep its
		-- ownership/replay fences, but allow captures during a yielding commit.
		performance = true,
		cancelEditorBinaryImport = true,
		cancelEditorReconcile = true,
		awaitEditorBinaryExport = true,
		readEditorBinaryExport = true,
		readEditorBinaryExportBatch = true,
		getEditorBinaryOverlayChunk = true,
	}
	Config.bridgeReplayProtectedMethods = {
		configurePropertyCandidates = true,
		setExportOptions = true,
		applyEditorChanges = true,
		beginEditorTransaction = true,
		beginEditorTransactionUpload = true,
		appendEditorTransactionUpload = true,
		finishEditorTransactionUpload = true,
		cancelEditorTransactionUpload = true,
		commitEditorTransaction = true,
		rollbackEditorTransaction = true,
		beginEditorBinaryImport = true,
		appendEditorBinaryImport = true,
		cancelEditorBinaryImport = true,
		cancelEditorReconcile = true,
		finishEditorBinaryImport = true,
		beginEditorPushReview = true,
		appendEditorPushReview = true,
		finishEditorPushReview = true,
		cancelEditorPushReview = true,
		requestEditorPushReview = true,
		requestProtectedWriteReview = true,
		getEditorPushReviewDecision = true,
		getEditorFilterCandidates = true,
		getEditorMutationPackages = true,
		setEditorPushReviewDecision = true,
		beginEditorBinaryExport = true,
		finishEditorBinaryExport = true,
		getStudioChangeState = true,
		sampleStudioProperty = true,
		getConsoleOutput = true,
		getGuiBounds = true,
		getMouseLocation = true,
		sendVirtualInput = true,
		deviceSimulator = true,
		networkSimulation = true,
		performance = true,
		captureViewportProbe = true,
		executeLuau = true,
		startStopPlay = true,
		getStudioState = true,
		getCreatorContext = true,
		cameraCapture = true,
		insertAsset = true,
		generateModel = true,
		creatorJob = true,
		multiEdit = true,
		uploadImages = true,
	}

	ConnectionModule.create({
		plugin = plugin,
		config = Config,
		ui = ui,
		settingsModule = SettingsModule,
		transportModule = TransportModule,
		httpService = HttpService,
		settingsPrefix = SETTINGS_PREFIX,
		runtimeSettings = initialRuntimeSettings,
		defaultHost = DEFAULT_HOST,
		defaultPorts = DEFAULT_PORTS,
		reconnectSeconds = RECONNECT_SECONDS,
		fastReconnectSeconds = FAST_RECONNECT_SECONDS,
		fastReconnectWindowSeconds = FAST_RECONNECT_WINDOW_SECONDS,
		connectSessionTimeoutSeconds = CONNECT_SESSION_TIMEOUT_SECONDS,
		debugBridgeConnection = DEBUG_BRIDGE_CONNECTION,
		maxRequestBytes = 16 * 1024 * 1024,
		maxQueuedExclusiveRequests = 16,
		allowedMethods = Config.bridgeMethodHandlers,
		isExclusiveMethod = function(method, params)
			return not not Config.bridgeExclusiveMethods[method]
				and not (method == "startStopPlay" and next(params) == nil)
		end,
		isSessionOwnedMethod = function(method)
			return not not Config.bridgeSessionOwnedMethods[method]
		end,
		isReplayProtectedMethod = function(method)
			return not not Config.bridgeReplayProtectedMethods[method]
		end,
		handleMethod = Config.handleMethod,
		updateStatusText = Config.updateStatusText,
		onRuntimeSettingsChanged = Config.applyBridgeRuntimeSettings,
		onRequestLeaseDisconnected = cancelRequestLeaseResources,
		acquireSessionLock = sessionLock.acquire,
		releaseSessionLock = sessionLock.release,
		inspectSessionLock = sessionLock.inspect,
		captureSessionLock = sessionLock.capture,
		validateSessionLock = sessionLock.validate,
		setExclusiveSessionGeneration = function(generation)
			activeExclusiveSessionGeneration = generation
		end,
		requestShutdown = function(unloading: boolean)
			editorSync.requestCancellation()
			local runtimeCleanupGeneration = RuntimeApi.requestCancellation()
			return function()
				if unloading and Config.studioChangeNotificationConnection then
					Config.studioChangeNotificationConnection:Disconnect()
					Config.studioChangeNotificationConnection = nil
				end
				if unloading then
					for _, connection in ipairs(lifetimeConnections) do
						connection:Disconnect()
					end
					table.clear(lifetimeConnections)
				end
				table.clear(Config.editorReviewUploads)
				Config.editorTransactionUploads.cleanup()
				editorSync.cleanup()
				if unloading then
					Config.studioChanges.stop()
				end
				table.clear(transactionExpectations)
				RuntimeApi.cleanup(runtimeCleanupGeneration)
				Config.creatorApi.cleanup()
				performance.cleanup()
				if unloading then
					networkSimulation.restore()
				end
				microProfiler.cleanup()
			end
		end,
	})
	local studioChangeStatusUpdatePending = false
	Config.studioChangeNotificationConnection = Config.studioChanges.onChanged(function()
		if studioChangeStatusUpdatePending then
			return
		end
		studioChangeStatusUpdatePending = true
		task.defer(function()
			studioChangeStatusUpdatePending = false
			Config.updateStatusText()
			local runtimeSettings = Config.getBridgeSettings()
			if runtimeSettings.notifications ~= false and not Config.hasOpenChannel() then
				local pendingCount = Config.studioChanges.pendingChangeCount()
				if pendingCount > 0 then
					local threshold = tonumber(runtimeSettings.changesThreshold) or 5
					local detail = if pendingCount > threshold
						then `{pendingCount} edits are waiting, above the review threshold of {threshold}.`
						else if pendingCount == 1
							then "One edit is waiting to sync."
							else `{pendingCount} edits are waiting to sync.`
					ui.notify(
						"disconnected-dirty",
						"Studio changes are waiting",
						detail,
						"Connect",
						Config.connectAll,
						true
					)
					return
				end
			end
			ui.dismissNotification("disconnected-dirty")
		end)
	end)
end

return BridgePluginRuntime
