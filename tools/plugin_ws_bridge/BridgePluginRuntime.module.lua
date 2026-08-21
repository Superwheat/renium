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
	scriptPaths: { string }?,
	scriptIndices: { number }?,
	scriptSources: { [string]: string },
	scriptSourcesByIndex: { [number]: string },
	scriptInstances: { [string]: LuaSourceContainer }?,
	scriptInstancesByIndex: { [number]: LuaSourceContainer }?,
	scriptKeyByInstance: { [Instance]: string },
	classDefaults: { [string]: any }?,
	classDefaultsEncoded: string?,
	scriptPathsEncoded: string?,
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
	modifiedDefaultAdaptiveStatsByKey: { [string]: any },
	modifiedDefaultAdaptiveDecisionByKey: { [string]: boolean },
	modifiedDefaultRuntimeDenylist: { [string]: boolean },
	exportMetrics: { [string]: number },
	exportMetricsSinceLastRead: { [string]: number },
}

local BridgePluginRuntime = {}

function BridgePluginRuntime.start(context)
	local plugin = context.plugin
	local rootScript = context.rootScript

	local HttpService = game:GetService("HttpService")
	local RunService = game:GetService("RunService")
	local ScriptEditorService = game:GetService("ScriptEditorService")
	local ChangeHistoryService = game:GetService("ChangeHistoryService")
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
	local NEXT_RUN_CLOSE_DELAY_SECONDS = 0.02
	local NEXT_RUN_RECONNECT_DELAY_SECONDS = 0.02
	local NEXT_RUN_FAST_WINDOW_SECONDS = 5.0
	local DEBUG_BRIDGE_CONNECTION = false
	local SERIALIZATION_BURST_BUDGET_SECONDS = 1 / 240
	local SERIALIZATION_BURST_CHECK_INTERVAL = 64
	local DEMAND_SERIALIZATION_BURST_BUDGET_SECONDS = 1 / 180
	local DEMAND_SERIALIZATION_BURST_CHECK_INTERVAL = 128
	local BALANCED_DEMAND_SERIALIZATION_BURST_BUDGET_SECONDS = 1 / 240
	local BALANCED_DEMAND_SERIALIZATION_BURST_CHECK_INTERVAL = 256
	local PARALLEL_SOURCE_BATCH_MIN_ITEMS = 24
	local BRIDGE_VERSION = "0.2.6"
	local BRIDGE_PROTOCOL_VERSION = "compact-v5"
	local BRIDGE_BUILD_UNIX = 1783875358
	local CHUNK_FRAME_PROTOCOL_VERSION = "rbs2"
	local COMPACT_VALUE_PROTOCOL_VERSION = "compact-v5-schema-4"
	local CLEAN_DEMAND_SERIALIZER_MAX_FRAME_MS = 33.0
	local THROTTLED_DEMAND_SERIALIZER_MAX_FRAME_MS = 100.0
	local DEFAULT_ACTIVE_DEMAND_SERIALIZERS = 2
	local MAX_ACTIVE_DEMAND_SERIALIZERS = 4
	local MAX_INSTANCE_BATCH_ITEMS = 5000
	local MAX_SOURCE_BATCH_PATHS = 1024
	local MAX_SOURCE_KEY_BYTES = 4096
	local DEFAULT_PERFORMANCE_MODE = "throughput"
	local MODIFIED_DEFAULT_BYPASS_ENABLED = plugin:GetSetting(SETTINGS_PREFIX .. "modifiedDefaultBypass") == true
	local SHAPE_COMPACT_INSTANCE_BATCHES = plugin:GetSetting(SETTINGS_PREFIX .. "shapeCompactInstanceBatches") ~= false
	local SHAPE_COMPACT_MIN_ITEMS = 128
	local SHAPE_COMPACT_MIN_CELL_SAVINGS = 32
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
	local ValueCodecModule = requireChildModule("BridgeValueCodec")
	local CODEC_VERSION = if ValueCodecModule.configureNativeNonFiniteJson(HttpService)
		then "compact-v5-schema-9"
		else "compact-v5-schema-8"
	local TransportModule = requireChildModule("BridgeTransport")
	local ConnectionModule = requireChildModule("BridgeConnection")
	local SessionLockModule = requireChildModule("BridgeSessionLock")
	local IdentityModule = requireChildModule("BridgeIdentity")
	local MaterialServiceModule = requireChildModule("BridgeMaterialService")
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
	}
	Config.shouldIgnoreInstance = sessionLock.isLockInstance
	Config.loadPendingStudioChanges = function()
		return plugin:GetSetting(SETTINGS_PREFIX .. "pendingStudioChanges")
	end
	Config.savePendingStudioChanges = function(services)
		plugin:SetSetting(SETTINGS_PREFIX .. "pendingStudioChanges", services)
	end
	Config.studioChanges = requireChildModule("BridgeStudioChanges").create(Config, ALLOWED_SERVICES)
	local RuntimeApi = requireChildModule("BridgeRuntimeApi").create(plugin, {
		runtimeId = Config.bridgeRuntimeId,
		assertSessionOwnership = function(sessionGeneration)
			if not sessionLock.validate(sessionGeneration or activeExclusiveSessionGeneration) then
				error("Renium session ownership was lost")
			end
		end,
		expectParentChange = Config.studioChanges.expectParentChange,
		expectPropertyEvent = Config.studioChanges.expectPropertyEvent,
		expectAttributeEvent = Config.studioChanges.expectAttributeEvent,
		expectTagChange = Config.studioChanges.expectTagChange,
		cancelExpectedEvent = Config.studioChanges.cancelExpectedEvent,
		studioChangeGeneration = Config.studioChanges.serviceGeneration,
	})
	Config.creatorApi = requireChildModule("BridgeCreatorApi").create()
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
	function Config.normalizePerformanceMode(raw: any): string
		if raw == "smooth" then
			return "smooth"
		elseif raw == "balanced" then
			return "balanced"
		end
		return DEFAULT_PERFORMANCE_MODE
	end
	local PERFORMANCE_MODE = Config.normalizePerformanceMode(plugin:GetSetting(SETTINGS_PREFIX .. "performanceMode"))
	local NOOP_EXPORT_YIELDER = function() end

	local function makeExportBurstYielder(checkInterval: number?, budgetSeconds: number?)
		if PERFORMANCE_MODE == "throughput" then
			return NOOP_EXPORT_YIELDER
		elseif PERFORMANCE_MODE == "balanced" then
			local interval = math.max(1, checkInterval or SERIALIZATION_BURST_CHECK_INTERVAL)
			local budget = math.max(budgetSeconds or SERIALIZATION_BURST_BUDGET_SECONDS, 1 / 120)
			return ParallelModule.makeBurstYielder(interval, budget)
		end

		return ParallelModule.makeBurstYielder(checkInterval, budgetSeconds)
	end

	local BUNDLED_PROPERTY_SCHEMAS_BY_CLASS: { [string]: { { any } } } =
		PropertySchemaModule.buildSchemasFromRbxDom(RbxDomDatabase, COMPACT_TYPE_IDS, StudioApiSchemaModule)
	local EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS: { [string]: { { any } } } = BUNDLED_PROPERTY_SCHEMAS_BY_CLASS
	local EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS: { [string]: { string } } =
		PropertySchemaModule.buildCandidatesFromSchemas(EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS)

	Config.studioChanges.configurePropertyCandidates(EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS)

	local stateByService: { [string]: ServiceState }
	local demandSerializerGate = Instance.new("BindableEvent")
	local activeDemandSerializers = 0
	stateByService = {}
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
		sourceUpdateAsync = 0,
		sourceDirect = 0,
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
	local historyVersion = 0
	lifetimeConnections[#lifetimeConnections + 1] = ChangeHistoryService.OnRecordingFinished:Connect(function()
		historyVersion += 1
	end)

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
		if serviceName == "ServerStorage" then
			return not sessionLock.isLockInstance(instance)
		end
		return serviceName ~= "Players"
			or not instance:IsA("Player") and not instance:FindFirstAncestorWhichIsA("Player")
	end
	Config.includeExportInstance = includeExportInstance

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

	function Config.showUndoNotification()
		local runtimeSettings = Config.getBridgeSettings()
		if runtimeSettings.notifications == false then
			return
		end
		local notificationHistoryVersion = historyVersion
		local canUndo, undoName = ChangeHistoryService:GetCanUndo()
		ui.notify(
			"undo",
			"Editor changes were applied",
			"Studio recorded the sync as one undo step.",
			"Undo",
			function()
				local stillCanUndo, currentUndoName = ChangeHistoryService:GetCanUndo()
				if
					canUndo
					and stillCanUndo
					and historyVersion == notificationHistoryVersion
					and currentUndoName == undoName
					and string.find(tostring(currentUndoName), "Renium", 1, true) ~= nil
				then
					ChangeHistoryService:Undo()
				else
					ui.notify(
						"undo-unavailable",
						"Undo is no longer available",
						"Studio has newer edits. Use the History panel to choose what to undo.",
						nil,
						nil,
						false
					)
				end
			end,
			false
		)
	end

	editorSync = EditorSyncModule.create({
		stats = editorSyncStats,
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
			local _, state = prepareService(serviceName, true, nil, scriptSourcesByInstance)
			return state
		end,
		includeExportInstance = includeExportInstance,
		getPropertySchema = function(className: string)
			return getClassPropertySchema(className) or {}
		end,
		getEnumValueNames = function(enumType: string)
			return Config.getEditorBinaryEnumValueNames(enumType)
		end,
		invalidateService = function(serviceName: string)
			stateByService[serviceName] = nil
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
		expectAttributeEvent = Config.studioChanges.expectAttributeEvent,
		expectTagChange = Config.studioChanges.expectTagChange,
		cancelExpectedEvent = Config.studioChanges.cancelExpectedEvent,
		beginStudioChangeSuppression = Config.studioChanges.beginSuppress,
		endStudioChangeSuppression = Config.studioChanges.endSuppress,
		beginStudioChangeJournal = Config.studioChanges.beginChangeJournal,
		drainStudioChangeJournal = Config.studioChanges.drainChangeJournal,
		finishStudioChangeJournal = Config.studioChanges.finishChangeJournal,
		studioChangeGeneration = Config.studioChanges.serviceGeneration,
		isStudioChangeTracking = Config.studioChanges.isTracking,
		hasNonArchivable = Config.studioChanges.hasNonArchivable,
		trackedExportInstances = Config.studioChanges.exportInstances,
	})
	local function tryReadModelPivotProperty(instance: Instance, propertyName: string): (boolean, any)
		if not (instance:IsA("Model") or instance:IsA("WorldModel")) then
			return false, nil
		end
		if propertyName == "Scale" then
			return true, (instance :: any):GetScale()
		elseif propertyName == "WorldPivotData" or propertyName == "WorldPivot" or propertyName == "Origin" then
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

	function Config.newExportMetrics(): { [string]: number }
		return {
			modifiedDefaultChecks = 0,
			modifiedDefaultElided = 0,
			modifiedDefaultValidationReads = 0,
			modifiedDefaultAdaptiveRejected = 0,
			modifiedDefaultRuntimeDenylistCount = 0,
			propertiesRead = 0,
			propertiesEncoded = 0,
			propertiesDefaultSkipped = 0,
			safeReadClassFallbackCount = 0,
			safeReadPropertyFallbackCount = 0,
		}
	end

	function Config.mergeExportMetrics(state: ServiceState, metrics: { [string]: number })
		for key, value in pairs(metrics) do
			if value ~= 0 then
				state.exportMetrics[key] = (state.exportMetrics[key] or 0) + value
				state.exportMetricsSinceLastRead[key] = (state.exportMetricsSinceLastRead[key] or 0) + value
			end
		end
	end

	function Config.bumpExportMetric(state: ServiceState, key: string, amount: number?)
		local delta = amount or 1
		state.exportMetrics[key] = (state.exportMetrics[key] or 0) + delta
		state.exportMetricsSinceLastRead[key] = (state.exportMetricsSinceLastRead[key] or 0) + delta
	end

	function Config.markModifiedDefaultRuntimeDenylist(state: ServiceState, bypassKey: string): boolean
		if not state.modifiedDefaultRuntimeDenylist[bypassKey] then
			state.modifiedDefaultRuntimeDenylist[bypassKey] = true
			return true
		end
		return false
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

	function Config.collectAndResetExportMetrics(): { [string]: number }
		local aggregated = Config.newExportMetrics()
		for _, state in pairs(stateByService) do
			for key, value in pairs(state.exportMetricsSinceLastRead) do
				if value ~= 0 then
					aggregated[key] = (aggregated[key] or 0) + value
					state.exportMetricsSinceLastRead[key] = 0
				end
			end
		end
		return aggregated
	end

	function Config.tryIsPropertyModified(instance: Instance, propertyName: string): (boolean, boolean?)
		local ok, modified = pcall(instance.IsPropertyModified, instance, propertyName)
		if ok and type(modified) == "boolean" then
			return true, modified
		end
		return false, nil
	end

	local MODIFIED_DEFAULT_BYPASS_PROPERTY_DENYLIST = {
		linkedsource = true,
	}
	local MODIFIED_DEFAULT_BYPASS_ADAPTIVE_SAMPLES_PER_KEY = 4
	local MODIFIED_DEFAULT_BYPASS_PROFIT_MARGIN = 1.25
	local MODIFIED_DEFAULT_BYPASS_MIN_SAVED_US = 0.25

	function Config.modifiedDefaultBypassKey(className: string, propertyName: string): string
		return className .. "." .. propertyName
	end

	function Config.canUseModifiedDefaultBypass(
		className: string,
		propertyName: string,
		typeId: number?,
		defaultComparable: any
	): boolean
		if not MODIFIED_DEFAULT_BYPASS_ENABLED then
			return false
		end
		if defaultComparable == nil then
			return false
		end
		local key = string.lower(propertyName)
		if MODIFIED_DEFAULT_BYPASS_PROPERTY_DENYLIST[key] then
			return false
		end
		if key == "rotation" and className == "Texture" then
			return false
		end
		return typeId ~= COMPACT_TYPE_IDS.Ref
	end

	function Config.evaluateModifiedDefaultBypass(
		state: ServiceState,
		bypassKey: string,
		instance: Instance,
		propertyName: string,
		defaultComparable: any,
		compareFn: any
	): (boolean, boolean?, boolean?, boolean, boolean, boolean)
		if state.modifiedDefaultRuntimeDenylist[bypassKey] then
			return false, nil, nil, false, false, false
		end

		local decision = state.modifiedDefaultAdaptiveDecisionByKey[bypassKey]
		if decision == true then
			return true, nil, nil, false, false, false
		elseif decision == false then
			return false, nil, nil, false, false, false
		end

		local modifiedStarted = os.clock()
		local hasModified, isModified = Config.tryIsPropertyModified(instance, propertyName)
		local modifiedUs = (os.clock() - modifiedStarted) * 1000000
		if not hasModified then
			state.modifiedDefaultAdaptiveDecisionByKey[bypassKey] = false
			Config.bumpExportMetric(state, "modifiedDefaultAdaptiveRejected")
			return false, hasModified, isModified, true, false, false
		end

		local readCompareStarted = os.clock()
		local gotSample, sampledValue = tryRead(instance, propertyName)
		local sampleIsDefault = gotSample
			and sampledValue ~= nil
			and compareFn
			and compareFn(sampledValue, defaultComparable, state)
		local readCompareUs = (os.clock() - readCompareStarted) * 1000000

		local stats = state.modifiedDefaultAdaptiveStatsByKey[bypassKey]
		if stats == nil then
			stats = {
				samples = 0,
				unmodified = 0,
				modifiedUs = 0,
				readCompareUs = 0,
			}
			state.modifiedDefaultAdaptiveStatsByKey[bypassKey] = stats
		end

		stats.samples += 1
		stats.modifiedUs += modifiedUs
		stats.readCompareUs += readCompareUs
		if not isModified then
			stats.unmodified += 1
		end

		if not isModified and not sampleIsDefault then
			state.modifiedDefaultAdaptiveDecisionByKey[bypassKey] = false
			local added = Config.markModifiedDefaultRuntimeDenylist(state, bypassKey)
			return false, hasModified, isModified, true, true, added
		end

		if stats.samples >= MODIFIED_DEFAULT_BYPASS_ADAPTIVE_SAMPLES_PER_KEY then
			local averageReadCompareUs = stats.readCompareUs / stats.samples
			local expectedSavedUs = averageReadCompareUs * stats.unmodified
			local expectedCheckUs = stats.modifiedUs
			local profitable = stats.unmodified > 0
				and expectedSavedUs > expectedCheckUs * MODIFIED_DEFAULT_BYPASS_PROFIT_MARGIN
				and (expectedSavedUs - expectedCheckUs) >= MODIFIED_DEFAULT_BYPASS_MIN_SAVED_US
			state.modifiedDefaultAdaptiveDecisionByKey[bypassKey] = profitable
			if not profitable then
				Config.bumpExportMetric(state, "modifiedDefaultAdaptiveRejected")
			end
			return profitable, hasModified, isModified, true, true, false
		end

		return false, hasModified, isModified, true, true, false
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

	local function deepEqual(a: any, b: any): boolean
		if a == b then
			return true
		end
		if type(a) ~= type(b) then
			return false
		end
		if type(a) ~= "table" then
			return false
		end
		for k, v in pairs(a) do
			if not deepEqual(v, b[k]) then
				return false
			end
		end
		for k, _ in pairs(b) do
			if a[k] == nil then
				return false
			end
		end
		return true
	end

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
			CLASS_PROPERTY_CANDIDATES_CACHE[className] = NO_PROPERTIES
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

	local function configurePropertyCandidates(payload: any): { [string]: any }
		if type(payload) ~= "table" then
			error("configurePropertyCandidates expects table payload")
		end

		local function sanitizeSchemaEntry(className: string, rawEntry: any): { any }?
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
			local enumType = if type(rawEnumType) == "string" then rawEnumType else false
			return { normalized, rawTypeId, enumType }
		end

		local configuredSchemas = {}
		local configuredClassCount = 0
		local configuredPropertyCount = 0
		for className, names in pairs(payload) do
			if type(className) ~= "string" or className == "" or type(names) ~= "table" then
				error("Property candidate classes must map non-empty class names to arrays")
			end
			local numericEntryCount = 0
			for key in pairs(names) do
				if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then
					error("Property candidate class entries must be dense arrays")
				end
				numericEntryCount += 1
			end
			if numericEntryCount == 0 or numericEntryCount ~= #names then
				error("Property candidate class entries must be non-empty dense arrays")
			end

			local sanitizedSchema = {}
			local seen: { [string]: boolean } = {}
			for _, rawEntry in ipairs(names) do
				local schemaEntry = sanitizeSchemaEntry(className, rawEntry)
				if schemaEntry then
					local key = propertyKey(schemaEntry[1])
					if not seen[key] then
						seen[key] = true
						sanitizedSchema[#sanitizedSchema + 1] = schemaEntry
					end
				end
			end
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
		local booleanOptions = {
			modifiedDefaultBypass = true,
			exportAllProperties = true,
		}
		for key, value in pairs(payload) do
			if booleanOptions[key] then
				if type(value) ~= "boolean" then
					error(key .. " must be a boolean")
				end
			elseif key == "performanceMode" then
				if value ~= "throughput" and value ~= "balanced" and value ~= "smooth" then
					error("performanceMode must be throughput, balanced, or smooth")
				end
			else
				error("Unknown export option " .. tostring(key))
			end
		end
		if payload.performanceMode ~= nil then
			PERFORMANCE_MODE = Config.normalizePerformanceMode(payload.performanceMode)
		end
		if payload.modifiedDefaultBypass ~= nil then
			local previousModifiedDefaultBypass = MODIFIED_DEFAULT_BYPASS_ENABLED
			MODIFIED_DEFAULT_BYPASS_ENABLED = payload.modifiedDefaultBypass
			if MODIFIED_DEFAULT_BYPASS_ENABLED ~= previousModifiedDefaultBypass then
				for _, serviceState in pairs(stateByService) do
					serviceState.hotPropertySchemaByClass = nil
				end
			end
		end
		if payload.exportAllProperties ~= nil then
			local previousExportAllProperties = EXPORT_ALL_PROPERTIES
			EXPORT_ALL_PROPERTIES = payload.exportAllProperties
			if EXPORT_ALL_PROPERTIES ~= previousExportAllProperties then
				table.clear(CLASS_PROPERTY_SCHEMA_CACHE)
				table.clear(CLASS_PROPERTY_CANDIDATES_CACHE)
				for _, serviceState in pairs(stateByService) do
					serviceState.hotPropertySchemaByClass = nil
				end
			end
		end
		plugin:SetSetting(SETTINGS_PREFIX .. "exportAllProperties", EXPORT_ALL_PROPERTIES)
		plugin:SetSetting(SETTINGS_PREFIX .. "performanceMode", PERFORMANCE_MODE)
		plugin:SetSetting(SETTINGS_PREFIX .. "modifiedDefaultBypass", MODIFIED_DEFAULT_BYPASS_ENABLED)
		Config.updateStatusText()
		return {
			exportAllProperties = EXPORT_ALL_PROPERTIES,
			performanceMode = PERFORMANCE_MODE,
			modifiedDefaultBypass = MODIFIED_DEFAULT_BYPASS_ENABLED,
		}
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

	local function getOrdinalPathSourceKey(state: ServiceState, instance: Instance): string
		local _, pathOrdinals = IdentityModule.getCachedRefPathParts(state, instance)
		local ordinals = table.create(#pathOrdinals)
		for index, ordinal in ipairs(pathOrdinals) do
			ordinals[index] = tostring(ordinal)
		end
		return "pathord:" .. table.concat(ordinals, ",") .. ":" .. IdentityModule.getCachedInstancePath(state, instance)
	end

	local function ensureScriptRangeIndex(state: ServiceState)
		if state.scriptIndices and state.scriptInstancesByIndex then
			return
		end
		local scriptIndices = table.create(#state.scriptObjects)
		local scriptInstancesByIndex = {}
		local yieldIfNeeded = makeExportBurstYielder()
		for _, inst in ipairs(state.scriptObjects) do
			local sourceIndex = IdentityModule.getCachedInstanceIndex(state, inst)
			if sourceIndex then
				scriptIndices[#scriptIndices + 1] = sourceIndex
				scriptInstancesByIndex[sourceIndex] = inst
			end
			yieldIfNeeded()
		end
		state.scriptIndices = scriptIndices
		state.scriptInstancesByIndex = scriptInstancesByIndex
	end

	local function ensureScriptKeyIndex(state: ServiceState)
		if state.scriptPaths and state.scriptInstances then
			return
		end
		local scriptPaths = table.create(#state.scriptObjects)
		local scriptInstances = {}
		local yieldIfNeeded = makeExportBurstYielder()
		for i, inst in ipairs(state.scriptObjects) do
			local sourceKey = IdentityModule.getCachedScriptSourceKey(state, inst)
			local pathSourceKey = "path:" .. IdentityModule.getCachedInstancePath(state, inst)
			local ordinalPathSourceKey = getOrdinalPathSourceKey(state, inst)
			scriptPaths[i] = sourceKey
			scriptInstances[sourceKey] = inst
			if not scriptInstances[pathSourceKey] then
				scriptInstances[pathSourceKey] = inst
			end
			scriptInstances[ordinalPathSourceKey] = inst
			yieldIfNeeded()
		end
		table.sort(scriptPaths)
		state.scriptPaths = scriptPaths
		state.scriptInstances = scriptInstances
		state.scriptPathsEncoded = nil
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
		local canModifiedBypass = table.create(propertyCount)
		local bypassKeys = table.create(propertyCount)
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
			canModifiedBypass[i] =
				Config.canUseModifiedDefaultBypass(className, propertyName, typeId, defaultComparable)
			bypassKeys[i] = if canModifiedBypass[i]
				then Config.modifiedDefaultBypassKey(className, propertyName)
				else false
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
			canModifiedBypass = canModifiedBypass,
			bypassKeys = bypassKeys,
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
					Config.bumpExportMetric(state, "safeReadPropertyFallbackCount")
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

	local function compactShapeKeyPart(value: any): string
		local valueType = type(value)
		if not value then
			return "f"
		elseif valueType == "number" then
			return "n:" .. tostring(value)
		elseif valueType == "string" then
			return "s:" .. value
		elseif valueType == "table" then
			local count = #value
			local parts = table.create(count)
			for i = 1, count do
				parts[i] = tostring(value[i] or 0)
			end
			return "t:" .. table.concat(parts, ",")
		end
		return valueType .. ":" .. tostring(value)
	end

	local function getCompactInstanceShapeId(
		shapes: { any },
		shapeIds: { [string]: number },
		classValue: any,
		mask: any
	): number
		local compactMask = mask or false
		local key = compactShapeKeyPart(classValue) .. "|" .. compactShapeKeyPart(compactMask)
		local existing = shapeIds[key]
		if existing then
			return existing
		end
		local nextId = #shapes + 1
		shapes[nextId] = { classValue, compactMask }
		shapeIds[key] = nextId
		return nextId
	end

	local function compactV5RowShapeKey(row: { any }): (string?, boolean)
		if type(row) ~= "table" or row[7] ~= nil then
			return nil, false
		end
		local classValue = row[2]
		if classValue == nil then
			return nil, false
		end
		local field4 = row[4]
		local field5 = row[5]
		local field6 = row[6]
		if field4 == nil or field5 == nil then
			return compactShapeKeyPart(classValue) .. "|f", false
		end
		if field6 == nil then
			local field4Type = type(field4)
			if field4Type ~= "number" and field4Type ~= "table" then
				return nil, false
			end
			return compactShapeKeyPart(classValue) .. "|" .. compactShapeKeyPart(field4), true
		end
		return compactShapeKeyPart(classValue) .. "|" .. compactShapeKeyPart(field5), true
	end

	local function shapeCompactV5Row(row: { any }, shapes: { any }, shapeIds: { [string]: number }): ({ any }?, boolean)
		if type(row) ~= "table" or row[7] ~= nil then
			return nil, false
		end

		local nameId = row[1]
		local classValue = row[2]
		local parentIndex = row[3] or false
		if classValue == nil then
			return nil, false
		end

		local field4 = row[4]
		local field5 = row[5]
		local field6 = row[6]
		if field4 == nil then
			local shapeId = getCompactInstanceShapeId(shapes, shapeIds, classValue, false)
			return { nameId, parentIndex, shapeId }, false
		end

		if field5 == nil then
			local shapeId = getCompactInstanceShapeId(shapes, shapeIds, classValue, false)
			return { nameId, parentIndex, shapeId, field4 }, false
		end

		if field6 == nil then
			local field4Type = type(field4)
			if field4Type ~= "number" and field4Type ~= "table" then
				return nil, false
			end
			local shapeId = getCompactInstanceShapeId(shapes, shapeIds, classValue, field4)
			return { nameId, parentIndex, shapeId, field5 }, true
		end

		local shapeId = getCompactInstanceShapeId(shapes, shapeIds, classValue, field5)
		return { nameId, parentIndex, shapeId, field4, field6 }, true
	end

	function Config.tryBuildCompactShapeBatch(items: { any }, count: number): ({ any }?, { any }?, number)
		if not SHAPE_COMPACT_INSTANCE_BATCHES or count < SHAPE_COMPACT_MIN_ITEMS then
			return nil, nil, 0
		end

		local shapeKeys = {}
		local shapeCount = 0
		local propertyRowCount = 0
		for i = 1, count do
			local shapeKey, rowHasPropertyMask = compactV5RowShapeKey(items[i])
			if not shapeKey then
				return nil, nil, 0
			end
			if not shapeKeys[shapeKey] then
				shapeKeys[shapeKey] = true
				shapeCount += 1
			end
			if rowHasPropertyMask then
				propertyRowCount += 1
			end
		end

		local estimatedCellSavings = propertyRowCount - (shapeCount * 2)
		if estimatedCellSavings < SHAPE_COMPACT_MIN_CELL_SAVINGS then
			return nil, nil, estimatedCellSavings
		end

		local shapedItems = table.create(count)
		local shapes = table.create(shapeCount)
		local shapeIds = {}
		for i = 1, count do
			local shapedRow = shapeCompactV5Row(items[i], shapes, shapeIds)
			if not shapedRow then
				return nil, nil, 0
			end
			shapedItems[i] = shapedRow
		end
		return shapedItems, shapes, estimatedCellSavings
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

	function Config.buildCompactV5Exporter(className: string, hotSchema: { [string]: any }, useFallbackMap: boolean?)
		local propertyCount = hotSchema.count
		local propertyNames = hotSchema.names
		local typeIds = hotSchema.typeIds
		local defaults = hotSchema.defaults
		local fastDefaults = hotSchema.fastDefaults
		local canModifiedBypass = hotSchema.canModifiedBypass
		local bypassKeys = hotSchema.bypassKeys
		local maskWordIndices = hotSchema.maskWordIndices
		local maskBitValues = hotSchema.maskBitValues
		local fastCompareModes = hotSchema.fastCompareModes
		local compareFns = hotSchema.compareFns
		local encodeFns = hotSchema.encodeFns
		local skipEncode = hotSchema.skipEncode
		local shouldUseFallbackMap = not not useFallbackMap
		local exportAllProperties = EXPORT_ALL_PROPERTIES
		local modifiedDefaultBypassEnabled = MODIFIED_DEFAULT_BYPASS_ENABLED and not exportAllProperties
		local evaluateModifiedDefaultBypass = Config.evaluateModifiedDefaultBypass
		local tryIsPropertyModified = Config.tryIsPropertyModified
		local getFallbackMap = Config.getClassPropertyFallbackMap
		local internBatchString = Config.internBatchString

		if not modifiedDefaultBypassEnabled then
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

				local compactMask = false
				if maskWords ~= nil and maskWordCount > 0 then
					if maskWordCount == 1 then
						compactMask = maskWords[1] or 0
					else
						local denseMask = table.create(maskWordCount)
						for i = 1, maskWordCount do
							denseMask[i] = maskWords[i] or 0
						end
						compactMask = denseMask
					end
				end

				local compactValues = if valuesOut ~= nil and valueWriteIndex > 0 then valuesOut else false

				if compactOverlay then
					if not compactMask and not compactValues then
						if not attributes then
							return false, 0, 0, 0, 0, 0, 0, 0
						end
						return {
							classValue,
							attributes,
						}, 0, 0, 0, 0, 0, 0, 0
					end
					if not attributes then
						return {
							classValue,
							false,
							compactMask,
							compactValues,
						},
							0,
							0,
							0,
							0,
							0,
							0,
							0
					end
					return {
						classValue,
						attributes,
						compactMask,
						compactValues,
					},
						0,
						0,
						0,
						0,
						0,
						0,
						0
				end

				local nameId = internBatchString(strings, stringIds, state.nameByIndex[instanceIndex] or instance.Name)
				if not compactMask and not compactValues then
					if not attributes then
						return {
							nameId,
							classValue,
							parentIndex or false,
						},
							0,
							0,
							0,
							0,
							0,
							0,
							0
					end
					return {
						nameId,
						classValue,
						parentIndex or false,
						attributes,
					},
						0,
						0,
						0,
						0,
						0,
						0,
						0
				end

				if not attributes then
					return {
						nameId,
						classValue,
						parentIndex or false,
						compactMask,
						compactValues,
					},
						0,
						0,
						0,
						0,
						0,
						0,
						0
				end

				return {
					nameId,
					classValue,
					parentIndex or false,
					attributes,
					compactMask,
					compactValues,
				},
					0,
					0,
					0,
					0,
					0,
					0,
					0
			end
		end

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
			local modifiedDefaultChecks = 0
			local modifiedDefaultElided = 0
			local modifiedDefaultValidationReads = 0
			local modifiedDefaultRuntimeDenylistCount = 0
			local propertiesRead = 0
			local propertiesEncoded = 0
			local propertiesDefaultSkipped = 0

			for i = 1, propertyCount do
				local propertyName = propertyNames[i]
				local defaultComparable = defaults[i]
				local skipRead = compactOverlay
					and hotSchema.nativeRefReadIndices
					and hotSchema.nativeRefReadIndices[i]
					and not Config.nativeRefSelectionContains(hotSchema.nativeRefReadIndices[i], instanceIndex)
				local originalNonArchivable = propertyName == "Archivable"
					and state.originalNonArchivableInstances
					and state.originalNonArchivableInstances[instance]
				if originalNonArchivable then
					skipRead = false
				end
				if
					not skipRead
					and not originalNonArchivable
					and modifiedDefaultBypassEnabled
					and not includeDefaults
				then
					local bypassKey = bypassKeys[i]
					if bypassKey and not state.modifiedDefaultRuntimeDenylist[bypassKey] and canModifiedBypass[i] then
						local shouldUseBypass, sampledHasModified, sampledIsModified, sampledCheck, sampledValidationRead, sampledDenylist =
							evaluateModifiedDefaultBypass(
								state,
								bypassKey,
								instance,
								propertyName,
								defaultComparable,
								compareFns[i]
							)
						if sampledCheck then
							modifiedDefaultChecks += 1
						end
						if sampledValidationRead then
							modifiedDefaultValidationReads += 1
						end
						if sampledDenylist then
							modifiedDefaultRuntimeDenylistCount += 1
						end

						local hasModified = sampledHasModified
						local isModified = sampledIsModified
						if shouldUseBypass and hasModified == nil then
							modifiedDefaultChecks += 1
							hasModified, isModified = tryIsPropertyModified(instance, propertyName)
						end
						if shouldUseBypass and hasModified and not isModified then
							skipRead = true
							if skipRead then
								modifiedDefaultElided += 1
							end
						end
					end
				end

				if not skipRead then
					propertiesRead += 1
					local value = nil
					local hasValue = false
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
					if originalNonArchivable then
						value = false
						hasValue = true
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
							local compareMode = fastCompareModes[i]
							local defaultFastComparable = fastDefaults[i]
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
							if not skipEncode[i] then
								local encodeFn = encodeFns[i]
								local encoded = if encodeFn then encodeFn(value, state, strings, stringIds) else nil
								if encoded ~= nil then
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
									propertiesEncoded += 1
								end
							end
						else
							propertiesDefaultSkipped += 1
						end
					end
				end
			end

			local compactMask = false
			if maskWords ~= nil and maskWordCount > 0 then
				if maskWordCount == 1 then
					compactMask = maskWords[1] or 0
				else
					local denseMask = table.create(maskWordCount)
					for i = 1, maskWordCount do
						denseMask[i] = maskWords[i] or 0
					end
					compactMask = denseMask
				end
			end

			local compactValues = if valuesOut ~= nil and valueWriteIndex > 0 then valuesOut else false

			if compactOverlay then
				if not compactMask and not compactValues then
					if not attributes then
						return false,
							modifiedDefaultChecks,
							modifiedDefaultElided,
							modifiedDefaultValidationReads,
							modifiedDefaultRuntimeDenylistCount,
							propertiesRead,
							propertiesEncoded,
							propertiesDefaultSkipped
					end
					return {
						classValue,
						attributes,
					},
						modifiedDefaultChecks,
						modifiedDefaultElided,
						modifiedDefaultValidationReads,
						modifiedDefaultRuntimeDenylistCount,
						propertiesRead,
						propertiesEncoded,
						propertiesDefaultSkipped
				end
				if not attributes then
					return {
						classValue,
						false,
						compactMask,
						compactValues,
					},
						modifiedDefaultChecks,
						modifiedDefaultElided,
						modifiedDefaultValidationReads,
						modifiedDefaultRuntimeDenylistCount,
						propertiesRead,
						propertiesEncoded,
						propertiesDefaultSkipped
				end
				return {
					classValue,
					attributes,
					compactMask,
					compactValues,
				},
					modifiedDefaultChecks,
					modifiedDefaultElided,
					modifiedDefaultValidationReads,
					modifiedDefaultRuntimeDenylistCount,
					propertiesRead,
					propertiesEncoded,
					propertiesDefaultSkipped
			end

			local nameId = internBatchString(strings, stringIds, state.nameByIndex[instanceIndex] or instance.Name)
			if not compactMask and not compactValues then
				if not attributes then
					return {
						nameId,
						classValue,
						parentIndex or false,
					},
						modifiedDefaultChecks,
						modifiedDefaultElided,
						modifiedDefaultValidationReads,
						modifiedDefaultRuntimeDenylistCount,
						propertiesRead,
						propertiesEncoded,
						propertiesDefaultSkipped
				end
				return {
					nameId,
					classValue,
					parentIndex or false,
					attributes,
				},
					modifiedDefaultChecks,
					modifiedDefaultElided,
					modifiedDefaultValidationReads,
					modifiedDefaultRuntimeDenylistCount,
					propertiesRead,
					propertiesEncoded,
					propertiesDefaultSkipped
			end

			if not attributes then
				return {
					nameId,
					classValue,
					parentIndex or false,
					compactMask,
					compactValues,
				},
					modifiedDefaultChecks,
					modifiedDefaultElided,
					modifiedDefaultValidationReads,
					modifiedDefaultRuntimeDenylistCount,
					propertiesRead,
					propertiesEncoded,
					propertiesDefaultSkipped
			end

			return {
				nameId,
				classValue,
				parentIndex or false,
				attributes,
				compactMask,
				compactValues,
			},
				modifiedDefaultChecks,
				modifiedDefaultElided,
				modifiedDefaultValidationReads,
				modifiedDefaultRuntimeDenylistCount,
				propertiesRead,
				propertiesEncoded,
				propertiesDefaultSkipped
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

	function Config.getDemandSerializerLimit(): number
		if PERFORMANCE_MODE == "throughput" then
			return MAX_ACTIVE_DEMAND_SERIALIZERS
		elseif PERFORMANCE_MODE == "balanced" then
			return DEFAULT_ACTIVE_DEMAND_SERIALIZERS
		end
		if not Config.perfState then
			return DEFAULT_ACTIVE_DEMAND_SERIALIZERS
		end
		local maxFrameMs = tonumber(Config.perfState.maxFrameMsSinceLastRead) or 0
		local lastFrameMs = tonumber(Config.perfState.lastFrameMs) or 0
		local sampleCountSinceLastRead = tonumber(Config.perfState.sampleCountSinceLastRead) or 0
		local stallCountOver50MsSinceLastRead = tonumber(Config.perfState.stallCountOver50MsSinceLastRead) or 0
		if
			maxFrameMs >= THROTTLED_DEMAND_SERIALIZER_MAX_FRAME_MS
			or lastFrameMs >= THROTTLED_DEMAND_SERIALIZER_MAX_FRAME_MS
		then
			return 1
		end
		if
			sampleCountSinceLastRead > 0
			and stallCountOver50MsSinceLastRead <= 0
			and maxFrameMs > 0
			and maxFrameMs <= CLEAN_DEMAND_SERIALIZER_MAX_FRAME_MS
			and lastFrameMs <= CLEAN_DEMAND_SERIALIZER_MAX_FRAME_MS
		then
			return MAX_ACTIVE_DEMAND_SERIALIZERS
		end
		return DEFAULT_ACTIVE_DEMAND_SERIALIZERS
	end

	function Config.shouldYieldDuringDemandSerialization(): boolean
		return PERFORMANCE_MODE ~= "throughput"
	end

	function Config.demandSerializationYieldConfig(): (number, number)
		if PERFORMANCE_MODE == "smooth" then
			return DEMAND_SERIALIZATION_BURST_CHECK_INTERVAL, DEMAND_SERIALIZATION_BURST_BUDGET_SECONDS
		end
		return BALANCED_DEMAND_SERIALIZATION_BURST_CHECK_INTERVAL, BALANCED_DEMAND_SERIALIZATION_BURST_BUDGET_SECONDS
	end

	function Config.acquireDemandSerializerSlot()
		while activeDemandSerializers >= Config.getDemandSerializerLimit() do
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
		nativeScriptSourcesOverride: { [Instance]: string }?
	): ({ [string]: any }?, ServiceState)
		if not ALLOWED_SERVICES[serviceName] then
			error("Unsupported service: " .. tostring(serviceName))
		end
		local service = game:GetService(serviceName)

		local nativeExport = nativeExportOnly == true
		local snapshotInstances = if nativeExport and nativeSnapshot then nativeSnapshot.instances else nil
		local descendants = snapshotInstances or service:GetDescendants()
		if not snapshotInstances then
			local excludedRoots = excludedExportRoots(serviceName, service)
			if #excludedRoots > 0 then
				local descendantCount = #descendants
				local includedCount = 0
				for index = 1, descendantCount do
					local instance = descendants[index]
					local included = true
					for _, root in ipairs(excludedRoots) do
						if instance == root or instance:IsDescendantOf(root) then
							included = false
							break
						end
					end
					if included then
						includedCount += 1
						descendants[includedCount] = instance
					end
				end
				for index = includedCount + 1, descendantCount do
					descendants[index] = nil
				end
			end
		end
		local expectedCount = if snapshotInstances then #snapshotInstances else #descendants + 1
		local instances = table.create(expectedCount)
		instances[1] = if snapshotInstances then snapshotInstances[1] else service
		local instanceCount = 1

		local scriptObjects = {}
		local scriptCount = 0
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
		if not snapshotInstances and not service.Archivable then
			nonArchivableInstance = service
			nonArchivableInstances[1] = service
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

		local yieldIfNeeded = if nativeExport then nil else makeExportBurstYielder()
		local firstDescendant = if snapshotInstances then 2 else 1
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
				if not inst.Archivable then
					nonArchivableInstance = nonArchivableInstance or inst
					nonArchivableInstances[#nonArchivableInstances + 1] = inst
				end
				local source = if nativeScriptSources then nativeScriptSources[inst] else nil
				if source ~= nil and Config.LUA_SOURCE_CLASS[className] then
					scriptCount += 1
					scriptObjects[scriptCount] = inst
					scriptSourcesByIndex[instanceCount] = source
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
				yieldIfNeeded()
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
			scriptPaths = nil,
			scriptIndices = nil,
			scriptSources = {},
			scriptSourcesByIndex = scriptSourcesByIndex,
			scriptInstances = nil,
			scriptInstancesByIndex = nil,
			scriptKeyByInstance = scriptKeyByInstance,
			classDefaults = nil,
			classDefaultsEncoded = nil,
			scriptPathsEncoded = nil,
			batchCacheByKey = {},
			batchCacheKeys = {},
			sourceBatchCacheByKey = {},
			sourceBatchCacheKeys = {},
			servicePropertySchemaByClass = nil,
			hotPropertySchemaByClass = nil,
			requiresPcallByClassProperty = {},
			modifiedDefaultAdaptiveStatsByKey = {},
			modifiedDefaultAdaptiveDecisionByKey = {},
			modifiedDefaultRuntimeDenylist = {},
			exportMetrics = Config.newExportMetrics(),
			exportMetricsSinceLastRead = Config.newExportMetrics(),
		}

		if nativeExport then
			return nil, state
		end

		stateByService[serviceName] = state
		local parentIndexYieldIfNeeded = makeExportBurstYielder()
		for _, index in ipairs(unresolvedParentIndices) do
			local parent = instances[index].Parent
			if parent ~= nil and parent ~= game then
				parentIndexByIndex[index] = instanceIdByInstance[parent] or false
			else
				parentIndexByIndex[index] = false
			end
			parentIndexYieldIfNeeded()
		end
		local scriptKeyYieldIfNeeded = makeExportBurstYielder()
		for _, inst in ipairs(scriptObjects) do
			IdentityModule.getCachedScriptSourceKey(state, inst)
			scriptKeyYieldIfNeeded()
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
			chunkFrameProtocolVersion = CHUNK_FRAME_PROTOCOL_VERSION,
			compactValueProtocolVersion = COMPACT_VALUE_PROTOCOL_VERSION,
			performanceMode = PERFORMANCE_MODE,
			bridgeRole = Config.bridgeRole,
			exportAllProperties = EXPORT_ALL_PROPERTIES,
			modifiedDefaultBypass = MODIFIED_DEFAULT_BYPASS_ENABLED,
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

	function Config.getNativeOverlayHotSchema(
		state: ServiceState,
		className: string,
		propertyNames: { any }?
	): { [string]: any }
		local source = Config.getHotPropertySchema(state, className)
		local requested = {}
		local requestedNativeRefIndices = {}
		if type(propertyNames) == "table" then
			for _, propertyEntry in ipairs(propertyNames) do
				if type(propertyEntry) == "string" then
					requested[propertyEntry] = 1
				elseif type(propertyEntry) == "table" and type(propertyEntry[1]) == "string" then
					if propertyEntry[2] == true then
						requested[propertyEntry[1]] = 2
					elseif type(propertyEntry[2]) == "table" then
						local rawSelection = propertyEntry[2]
						if type(rawSelection.packed) == "string" then
							local packed = game:GetService("EncodingService")
								:Base64Decode(buffer.fromstring(rawSelection.packed))
							local count = tonumber(rawSelection.count)
							if not count or count < 1 or count % 1 ~= 0 or buffer.len(packed) ~= count * 3 then
								error("Invalid native reference selection")
							end
							requested[propertyEntry[1]] = 3
							local indices = table.create(count)
							for index = 1, count do
								local offset = (index - 1) * 3
								indices[index] = buffer.readu8(packed, offset)
									+ bit32.lshift(buffer.readu8(packed, offset + 1), 8)
									+ bit32.lshift(buffer.readu8(packed, offset + 2), 16)
							end
							requestedNativeRefIndices[propertyEntry[1]] = {
								packed = packed,
								offset = 0,
								candidate = buffer.readu8(packed, 0)
									+ bit32.lshift(buffer.readu8(packed, 1), 8)
									+ bit32.lshift(buffer.readu8(packed, 2), 16),
								indices = indices,
							}
						else
							local indices = {}
							local selectedIndices = {}
							for _, rawIndex in ipairs(rawSelection) do
								local index = tonumber(rawIndex)
								if index and index % 1 == 0 and index > 0 and not indices[index] then
									indices[index] = true
									selectedIndices[#selectedIndices + 1] = index
								end
							end
							if next(indices) then
								table.sort(selectedIndices)
								indices.indices = selectedIndices
								requested[propertyEntry[1]] = 3
								requestedNativeRefIndices[propertyEntry[1]] = indices
							end
						end
					else
						requested[propertyEntry[1]] = 1
					end
				end
			end
		end
		local fields = {
			"typeIds",
			"enumTypes",
			"defaults",
			"fastDefaults",
			"canModifiedBypass",
			"bypassKeys",
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
			canModifiedBypass = {},
			bypassKeys = {},
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
		shapeBatchesEnabled: boolean?,
		stableIdsEnabled: boolean?,
		overlayId: string?,
		overlayVariant: string?
	): string
		local key = ChunkingModule.getCompactInstanceBatchCacheKey(startIndex, maxCount)
		if shapeBatchesEnabled then
			key ..= ":shape-v1"
		else
			key ..= ":plain-v1"
		end
		if stableIdsEnabled then
			key ..= ":stable-v1"
		end
		if type(overlayId) == "string" and overlayId ~= "" then
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
		shapeBatchesEnabled: boolean?,
		stableIdsEnabled: boolean?,
		overlayPropertiesByClass: { [string]: { any } }?,
		overlayId: string?,
		overlayVariant: string?,
		stateOverride: ServiceState?
	): (string, number)
		local state = stateOverride or getState(serviceName)
		local useShapeBatches = not not shapeBatchesEnabled
		local includeStableIds = not not stableIdsEnabled
		local key = Config.getCompactInstanceBatchVariantCacheKey(
			startIndex,
			maxCount,
			useShapeBatches,
			includeStableIds,
			overlayId,
			overlayVariant
		)
		local cachedPayload = state.batchCacheByKey[key]
		if cachedPayload then
			return cachedPayload, 0
		end

		local instances = state.instances
		local total = #instances
		local startPos = Config.boundedPositiveInteger(startIndex, 1, math.max(total + 1, 1))
		local maximumItems = if type(overlayId) == "string" and overlayId ~= ""
			then math.max(total, 1)
			else MAX_INSTANCE_BATCH_ITEMS
		local take = Config.boundedPositiveInteger(maxCount, 300, maximumItems)

		local function buildPayload(): { [string]: any }
			if startPos > total then
				return {
					format = BRIDGE_PROTOCOL_VERSION,
					codecVersion = CODEC_VERSION,
					total = total,
					strings = {},
					debugIds = if includeStableIds then {} else nil,
					items = {},
				}
			end

			local finish = math.min(total, startPos + take - 1)
			local count = finish - startPos + 1
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
			elseif MODIFIED_DEFAULT_BYPASS_ENABLED then
				local workerMetrics = {}
				ParallelModule.runParallelChunks(count, 1, function(startOffset, endOffset)
					local modifiedDefaultChecks = 0
					local modifiedDefaultElided = 0
					local modifiedDefaultValidationReads = 0
					local modifiedDefaultRuntimeDenylistCount = 0
					local propertiesRead = 0
					local propertiesEncoded = 0
					local propertiesDefaultSkipped = 0
					local lastClassName = nil
					local lastHotSchema = nil
					local yieldIfNeeded = nil
					if Config.shouldYieldDuringDemandSerialization() then
						local checkInterval, budgetSeconds = Config.demandSerializationYieldConfig()
						yieldIfNeeded = ParallelModule.makeBurstYielder(checkInterval, budgetSeconds)
					end
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
							local item, itemModifiedChecks, itemModifiedElided, itemModifiedValidationReads, itemModifiedRuntimeDenylistCount, itemPropertiesRead, itemPropertiesEncoded, itemPropertiesDefaultSkipped =
								Config.exportCompactV5InstanceIndexed(
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
							items[offset] = item
							modifiedDefaultChecks += itemModifiedChecks or 0
							modifiedDefaultElided += itemModifiedElided or 0
							modifiedDefaultValidationReads += itemModifiedValidationReads or 0
							modifiedDefaultRuntimeDenylistCount += itemModifiedRuntimeDenylistCount or 0
							propertiesRead += itemPropertiesRead or 0
							propertiesEncoded += itemPropertiesEncoded or 0
							propertiesDefaultSkipped += itemPropertiesDefaultSkipped or 0
						end
						if yieldIfNeeded then
							yieldIfNeeded()
						end
					end
					workerMetrics[startOffset] = {
						modifiedDefaultChecks = modifiedDefaultChecks,
						modifiedDefaultElided = modifiedDefaultElided,
						modifiedDefaultValidationReads = modifiedDefaultValidationReads,
						modifiedDefaultRuntimeDenylistCount = modifiedDefaultRuntimeDenylistCount,
						propertiesRead = propertiesRead,
						propertiesEncoded = propertiesEncoded,
						propertiesDefaultSkipped = propertiesDefaultSkipped,
					}
				end)
				local mergedMetrics = Config.newExportMetrics()
				for _, metrics in pairs(workerMetrics) do
					for metricKey, value in pairs(metrics) do
						mergedMetrics[metricKey] = (mergedMetrics[metricKey] or 0) + value
					end
				end
				Config.mergeExportMetrics(state, mergedMetrics)
			else
				ParallelModule.runParallelChunks(count, 1, function(startOffset, endOffset)
					local lastClassName = nil
					local lastHotSchema = nil
					local yieldIfNeeded = nil
					if Config.shouldYieldDuringDemandSerialization() then
						local checkInterval, budgetSeconds = Config.demandSerializationYieldConfig()
						yieldIfNeeded = ParallelModule.makeBurstYielder(checkInterval, budgetSeconds)
					end
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
						if yieldIfNeeded then
							yieldIfNeeded()
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
					items = classGroups,
				}
			end
			if useShapeBatches then
				local shapedItems, shapes = Config.tryBuildCompactShapeBatch(items, count)
				if shapedItems and shapes then
					return {
						format = "compact-v5-shape",
						codecVersion = CODEC_VERSION,
						total = total,
						strings = strings,
						shapes = shapes,
						debugIds = debugIds,
						items = shapedItems,
					}
				end
			end

			return {
				format = BRIDGE_PROTOCOL_VERSION,
				codecVersion = CODEC_VERSION,
				total = total,
				strings = strings,
				debugIds = debugIds,
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

	function Config.getClassDefaults(serviceName: string): (string, number)
		local state = getState(serviceName)
		if state.classDefaultsEncoded then
			return state.classDefaultsEncoded, 0
		end
		if not state.classDefaults then
			local classDefaults = {}
			for _, className in ipairs(state.classNames) do
				local defaults = getDefaultSerializedProperties(className)
				if defaults and next(defaults) then
					classDefaults[className] = defaults
				end
			end
			state.classDefaults = classDefaults
		end
		local encoded, encodeMs = ChunkingModule.jsonEncodeTimed(state.classDefaults)
		state.classDefaultsEncoded = encoded
		return state.classDefaultsEncoded, encodeMs
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

	function Config.getScriptPaths(serviceName: string): (string, number)
		local state = getState(serviceName)
		ensureScriptKeyIndex(state)
		if not state.scriptPathsEncoded then
			local encoded, encodeMs = ChunkingModule.jsonEncodeTimed(state.scriptPaths)
			state.scriptPathsEncoded = encoded
			return state.scriptPathsEncoded, encodeMs
		end
		return state.scriptPathsEncoded, 0
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

	function Config.getSourceForKey(state: ServiceState, sourceKey: string): string
		local src = state.scriptSources[sourceKey]
		if src then
			return src
		end

		local scriptInstance = state.scriptInstances and state.scriptInstances[sourceKey] or nil
		src = Config.readScriptSource(scriptInstance, "script source " .. sourceKey)
		state.scriptSources[sourceKey] = src
		return src
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

	function Config.getSourceChunk(
		serviceName: string,
		instancePath: string,
		startIndex: number?,
		maxLen: number?
	): { [string]: any }
		local state = getState(serviceName)
		ensureScriptKeyIndex(state)
		if #instancePath > MAX_SOURCE_KEY_BYTES then
			error("Source key exceeds safe size limit")
		end
		return ChunkingModule.chunkEncodedString(Config.getSourceForKey(state, instancePath), startIndex, maxLen, 0)
	end

	function Config.getSourceBatchEncoded(serviceName: string, instancePaths: { any }): (string, number)
		local state = getState(serviceName)
		ensureScriptKeyIndex(state)
		if type(instancePaths) ~= "table" or #instancePaths > MAX_SOURCE_BATCH_PATHS then
			error("Source batch has too many paths")
		end
		local normalizedPaths = table.create(#instancePaths)
		for i, value in ipairs(instancePaths) do
			local sourceKey = tostring(value)
			if #sourceKey > MAX_SOURCE_KEY_BYTES then
				error("Source batch key exceeds safe size limit")
			end
			normalizedPaths[i] = sourceKey
		end
		local cacheKey = ChunkingModule.getSourceBatchCacheKey(normalizedPaths)
		local cachedPayload = state.sourceBatchCacheByKey[cacheKey]
		if cachedPayload then
			return cachedPayload, 0
		end

		local out = {}
		local sourcesByIndex = table.create(#normalizedPaths)
		local workerCount =
			ParallelModule.getParallelChunkWorkerCount(#normalizedPaths, PARALLEL_SOURCE_BATCH_MIN_ITEMS)
		ParallelModule.runParallelChunks(#normalizedPaths, workerCount, function(startIndex, endIndex)
			for i = startIndex, endIndex do
				local sourceKey = normalizedPaths[i]
				sourcesByIndex[i] = Config.getSourceForKey(state, sourceKey)
			end
		end)
		for i, sourceKey in ipairs(normalizedPaths) do
			out[sourceKey] = sourcesByIndex[i]
		end

		local encoded, encodeMs = ChunkingModule.jsonEncodeTimed(out)
		Config.cacheBatchPayload(state.sourceBatchCacheByKey, state.sourceBatchCacheKeys, cacheKey, encoded, 64)
		return encoded, encodeMs
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

	function Config.getSourceBatchChunk(
		serviceName: string,
		instancePaths: { any },
		startIndex: number?,
		maxLen: number?
	): { [string]: any }
		local encoded, encodeMs = Config.getSourceBatchEncoded(serviceName, instancePaths)
		return ChunkingModule.chunkEncodedString(encoded, startIndex, maxLen, encodeMs)
	end

	function Config.getInstanceBatchCompactChunk(
		serviceName: string,
		startIndex: number?,
		maxCount: number?,
		chunkStart: number?,
		maxLen: number?,
		shapeBatchesEnabled: boolean?,
		stableIdsEnabled: boolean?,
		overlayPropertiesByClass: { [string]: { any } }?,
		overlayId: string?,
		overlayVariant: string?,
		stateOverride: ServiceState?
	): { [string]: any }
		local encoded, encodeMs = Config.getInstanceBatchCompact(
			serviceName,
			startIndex,
			maxCount,
			not not shapeBatchesEnabled,
			not not stableIdsEnabled,
			overlayPropertiesByClass,
			overlayId,
			overlayVariant,
			stateOverride
		)
		local result = ChunkingModule.chunkEncodedString(encoded, chunkStart, maxLen, encodeMs)
		if type(overlayId) == "string" and overlayId ~= "" and result.nextStart > result.total then
			local state = stateOverride or getState(serviceName)
			local key = Config.getCompactInstanceBatchVariantCacheKey(
				startIndex,
				maxCount,
				not not shapeBatchesEnabled,
				not not stableIdsEnabled,
				overlayId,
				overlayVariant
			)
			state.batchCacheByKey[key] = nil
			for index, cachedKey in ipairs(state.batchCacheKeys) do
				if cachedKey == key then
					table.remove(state.batchCacheKeys, index)
					break
				end
			end
		end
		return result
	end

	function Config.getClassDefaultsChunk(serviceName: string, startIndex: number?, maxLen: number?): { [string]: any }
		local encoded, encodeMs = Config.getClassDefaults(serviceName)
		return ChunkingModule.chunkEncodedString(encoded, startIndex, maxLen, encodeMs)
	end

	function Config.getScriptPathsChunk(serviceName: string, startIndex: number?, maxLen: number?): { [string]: any }
		local encoded, encodeMs = Config.getScriptPaths(serviceName)
		return ChunkingModule.chunkEncodedString(encoded, startIndex, maxLen, encodeMs)
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

	Config.bridgeMethodHandlers.getPerformanceStats = function()
		local exportMetrics = Config.collectAndResetExportMetrics()
		local stats = {
			fps = Config.perfState.fps,
			frameMs = Config.perfState.frameMs,
			lastFrameMs = Config.perfState.lastFrameMs,
			maxFrameMs = Config.perfState.maxFrameMsSinceLastRead,
			lastHeartbeat = Config.perfState.lastHeartbeat,
			sampleCount = Config.perfState.sampleCount,
			sampleCountSinceLastRead = Config.perfState.sampleCountSinceLastRead,
			stallCountOver33Ms = Config.perfState.stallCountOver33MsSinceLastRead,
			stallCountOver50Ms = Config.perfState.stallCountOver50MsSinceLastRead,
			stallCountOver100Ms = Config.perfState.stallCountOver100MsSinceLastRead,
			modifiedDefaultChecks = exportMetrics.modifiedDefaultChecks,
			modifiedDefaultElided = exportMetrics.modifiedDefaultElided,
			modifiedDefaultValidationReads = exportMetrics.modifiedDefaultValidationReads,
			modifiedDefaultRuntimeDenylistCount = exportMetrics.modifiedDefaultRuntimeDenylistCount,
			propertiesRead = exportMetrics.propertiesRead,
			propertiesEncoded = exportMetrics.propertiesEncoded,
			propertiesDefaultSkipped = exportMetrics.propertiesDefaultSkipped,
			safeReadClassFallbackCount = exportMetrics.safeReadClassFallbackCount,
			safeReadPropertyFallbackCount = exportMetrics.safeReadPropertyFallbackCount,
			editorSync = editorSyncStats,
		}
		Config.perfState.maxFrameMsSinceLastRead = 0
		Config.perfState.sampleCountSinceLastRead = 0
		Config.perfState.stallCountOver33MsSinceLastRead = 0
		Config.perfState.stallCountOver50MsSinceLastRead = 0
		Config.perfState.stallCountOver100MsSinceLastRead = 0
		return stats
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
	Config.bridgeMethodHandlers.getEditorServiceChangeGenerations = editorSync.getServiceChangeGenerations
	Config.editorTransactionUploads = TransactionUploadModule.create(editorSync.beginTransaction, function(id, params)
		transactionExpectations[id] = params
	end)
	Config.bridgeMethodHandlers.beginEditorTransactionUpload = Config.editorTransactionUploads.begin
	Config.bridgeMethodHandlers.appendEditorTransactionUpload = Config.editorTransactionUploads.append
	Config.bridgeMethodHandlers.finishEditorTransactionUpload = Config.editorTransactionUploads.finish
	Config.bridgeMethodHandlers.cancelEditorTransactionUpload = Config.editorTransactionUploads.cancel
	Config.bridgeMethodHandlers.beginEditorTransaction = function(p)
		local result = editorSync.beginTransaction(p)
		transactionExpectations[tostring(p.transactionId or "")] = p
		return result
	end
	Config.bridgeMethodHandlers.commitEditorTransaction = function(p)
		local transactionId = tostring(p.transactionId or "")
		Config.studioChanges.beginSuppress(nil, transactionExpectations[transactionId])
		local ok, result = pcall(editorSync.commitTransaction, p)
		task.defer(Config.studioChanges.endSuppress)
		if not ok then
			error(result, 0)
		end
		transactionExpectations[transactionId] = nil
		if result.undoRecorded == true then
			Config.showUndoNotification()
		end
		return result
	end
	Config.bridgeMethodHandlers.rollbackEditorTransaction = function(p)
		local transactionId = tostring(p.transactionId or "")
		Config.studioChanges.beginSuppress(nil)
		local ok, result = pcall(editorSync.rollbackTransaction, p)
		task.defer(Config.studioChanges.endSuppress)
		transactionExpectations[transactionId] = nil
		if not ok then
			error(result, 0)
		end
		return result
	end

	Config.bridgeMethodHandlers.finishEditorBinaryImport = function(p)
		Config.studioChanges.beginSuppress(nil)
		local ok, result = pcall(editorSync.finishBinaryImport, p)
		task.defer(Config.studioChanges.endSuppress)
		if not ok then
			error(result, 0)
		end
		return result
	end

	Config.bridgeMethodHandlers.applyEditorChanges = function(p)
		Config.studioChanges.beginSuppress(nil, p)
		local ok, result = pcall(editorSync.applyChanges, p)
		task.defer(Config.studioChanges.endSuppress)
		if not ok then
			error(result, 0)
		end
		if result.ok == true and result.undoRecorded == true and tostring(p.transactionId or "") == "" then
			Config.showUndoNotification()
		end
		return result
	end

	Config.bridgeMethodHandlers.getStudioChangeState = function(p)
		local runtimeSettings = Config.getBridgeSettings()
		if tostring(p.runtimeId or "") == Config.bridgeRuntimeId then
			Config.ackPendingBridgeSettingChanges(p.ackRuntimeSettingsSeq)
		end
		local runtimeSettingChanges, runtimeSettingsSeq = Config.getPendingBridgeSettingChanges()
		if runtimeSettings.twoWaySync == false then
			return {
				ok = true,
				tracking = false,
				role = Config.bridgeRole,
				dirtyServices = {},
				fullSyncServices = {},
				propertyChanges = {},
				changes = {},
				twoWaySyncEnabled = false,
				runtimeSettingChanges = runtimeSettingChanges,
				runtimeSettingsSeq = runtimeSettingsSeq,
				runtimeId = Config.bridgeRuntimeId,
				editorActions = pendingEditorActions(p.ackEditorActions, p.runtimeId),
			}
		end
		local changeState = Config.studioChanges.getState(p)
		changeState.twoWaySyncEnabled = true
		changeState.runtimeSettingChanges = runtimeSettingChanges
		changeState.runtimeSettingsSeq = runtimeSettingsSeq
		changeState.editorActions = pendingEditorActions(p.ackEditorActions, p.runtimeId)
		return changeState
	end

	Config.bridgeMethodHandlers.setConflictResolution = function(p)
		if type(p.value) ~= "string" then
			error("setConflictResolution requires a string value")
		end
		local conflictResolution = Config.studioChanges.setConflictResolution(p.value)
		SettingsModule.saveConflictResolution(plugin, SETTINGS_PREFIX, conflictResolution)
		return { ok = true, conflictResolution = conflictResolution }
	end

	Config.bridgeMethodHandlers.getConsoleOutput = RuntimeApi.getConsoleOutput
	Config.bridgeMethodHandlers.getGuiBounds = RuntimeApi.getGuiBounds
	Config.bridgeMethodHandlers.getGuiInventory = RuntimeApi.getGuiInventory
	Config.bridgeMethodHandlers.getWorldPoint = RuntimeApi.getWorldPoint
	Config.bridgeMethodHandlers.getMouseLocation = RuntimeApi.getMouseLocation
	Config.bridgeMethodHandlers.sendVirtualInput = RuntimeApi.sendVirtualInput
	Config.bridgeMethodHandlers.deviceSimulator = RuntimeApi.deviceSimulator
	Config.bridgeMethodHandlers.captureViewportProbe = RuntimeApi.captureViewportProbe
	Config.bridgeMethodHandlers.executeLuau = RuntimeApi.executeLuau
	Config.bridgeMethodHandlers.cancelLuauExecution = function(p, sessionGeneration)
		return RuntimeApi.cancelLuauExecution(p, sessionGeneration)
	end
	Config.bridgeMethodHandlers.startStopPlay = RuntimeApi.startStopPlay
	Config.bridgeMethodHandlers.getStudioState = Config.creatorApi.studioState
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

	Config.bridgeMethodHandlers.prepareForNextRun = function()
		return "ok"
	end

	Config.bridgeMethodHandlers.prepare = function(p)
		return prepareService(tostring(p.service))
	end

	Config.bridgeMethodHandlers.getInstanceBatchCompactChunk = function(p)
		return Config.getInstanceBatchCompactChunk(
			tostring(p.service),
			p.startIndex,
			p.maxCount,
			p.chunkStart,
			p.maxLen,
			true,
			true
		)
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
		local state = editorSync.getBinaryExportState(overlayId, serviceName)
		local started = os.clock()
		local result = Config.getInstanceBatchCompactChunk(
			serviceName,
			p.startIndex,
			p.maxCount,
			p.chunkStart,
			p.maxLen,
			false,
			p.supportsStableInstanceIds ~= false,
			p.overlayPropertiesByClass,
			overlayId,
			tostring(p.overlayVariant or ""),
			state
		)
		editorSync.validateBinaryExportState(overlayId, serviceName)
		result.pluginServerMs = math.max(0, (os.clock() - started) * 1000 - (result.pluginEncodeMs or 0))
		return result
	end

	Config.bridgeMethodHandlers.getClassDefaultsChunk = function(p)
		return Config.getClassDefaultsChunk(tostring(p.service), p.startIndex, p.maxLen)
	end

	Config.bridgeMethodHandlers.getScriptPathsChunk = function(p)
		return Config.getScriptPathsChunk(tostring(p.service), p.startIndex, p.maxLen)
	end

	Config.bridgeMethodHandlers.getSourceBatchChunk = function(p)
		return Config.getSourceBatchChunk(tostring(p.service), p.instancePaths or {}, p.startIndex, p.maxLen)
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

	Config.bridgeMethodHandlers.getSourceChunk = function(p)
		return Config.getSourceChunk(tostring(p.service), tostring(p.instancePath), p.startIndex, p.maxLen)
	end

	Config.bridgeMethodHandlers.getLiveSourceBatch = function(p)
		return editorSync.getLiveSourceBatch(p)
	end

	Config.bridgeMethodHandlers.release = function(p)
		stateByService[tostring(p.service)] = nil
		return "ok"
	end

	function Config.handleMethod(method: string, params: { [string]: any }, sessionGeneration: number?): any
		local handler = Config.bridgeMethodHandlers[method]
		if not handler then
			error("Unknown method: " .. tostring(method))
		end
		return handler(params, sessionGeneration)
	end

	Config.perfState = {
		fps = 60.0,
		frameMs = 16.67,
		lastFrameMs = 16.67,
		maxFrameMsSinceLastRead = 16.67,
		lastHeartbeat = os.clock(),
		sampleCount = 0,
		sampleCountSinceLastRead = 0,
		stallCountOver33MsSinceLastRead = 0,
		stallCountOver50MsSinceLastRead = 0,
		stallCountOver100MsSinceLastRead = 0,
	}

	lifetimeConnections[#lifetimeConnections + 1] = RunService.Heartbeat:Connect(function(dt: number)
		if dt <= 0 then
			return
		end
		local frameMs = dt * 1000
		local instantFps = 1 / dt
		local alpha = 0.08
		Config.perfState.fps = Config.perfState.fps + (instantFps - Config.perfState.fps) * alpha
		if Config.perfState.fps <= 0 then
			Config.perfState.fps = instantFps
		end
		Config.perfState.frameMs = 1000 / Config.perfState.fps
		Config.perfState.lastFrameMs = frameMs
		Config.perfState.maxFrameMsSinceLastRead = math.max(Config.perfState.maxFrameMsSinceLastRead or 0, frameMs)
		Config.perfState.lastHeartbeat = os.clock()
		Config.perfState.sampleCount += 1
		Config.perfState.sampleCountSinceLastRead += 1
		if frameMs > 33 then
			Config.perfState.stallCountOver33MsSinceLastRead += 1
		end
		if frameMs > 50 then
			Config.perfState.stallCountOver50MsSinceLastRead += 1
		end
		if frameMs > 100 then
			Config.perfState.stallCountOver100MsSinceLastRead += 1
		end
	end)

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
		setConflictResolution = true,
		deviceSimulator = true,
		captureViewportProbe = true,
		sendVirtualInput = true,
		executeLuau = true,
		startStopPlay = true,
		cameraCapture = true,
		insertAsset = true,
		generateModel = true,
		multiEdit = true,
		uploadImages = true,
		prepareForNextRun = true,
		prepare = true,
		release = true,
	}
	Config.bridgeSessionOwnedMethods = {
		cancelEditorBinaryImport = true,
		cancelEditorReconcile = true,
		cancelLuauExecution = true,
		awaitEditorBinaryExport = true,
		readEditorBinaryExport = true,
		readEditorBinaryExportBatch = true,
		getEditorBinaryOverlayChunk = true,
	}
	Config.bridgeReplayProtectedMethods = {
		getPerformanceStats = true,
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
		setEditorPushReviewDecision = true,
		beginEditorBinaryExport = true,
		finishEditorBinaryExport = true,
		getStudioChangeState = true,
		setConflictResolution = true,
		getConsoleOutput = true,
		getGuiBounds = true,
		getMouseLocation = true,
		sendVirtualInput = true,
		deviceSimulator = true,
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
		prepareForNextRun = true,
		prepare = true,
		release = true,
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
		nextRunCloseDelaySeconds = NEXT_RUN_CLOSE_DELAY_SECONDS,
		nextRunReconnectDelaySeconds = NEXT_RUN_RECONNECT_DELAY_SECONDS,
		nextRunFastWindowSeconds = NEXT_RUN_FAST_WINDOW_SECONDS,
		debugBridgeConnection = DEBUG_BRIDGE_CONNECTION,
		maxRequestBytes = 16 * 1024 * 1024,
		maxQueuedExclusiveRequests = 16,
		allowedMethods = Config.bridgeMethodHandlers,
		isExclusiveMethod = function(method)
			return not not Config.bridgeExclusiveMethods[method]
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
		getFinalConsoleSnapshot = function()
			local info = Config.getBridgeInfo()
			if
				info.launchNonce == nil
				or info.launchNonce == ""
				or (info.bridgeRole ~= "play-server" and info.bridgeRole ~= "play-client")
			then
				return nil
			end
			return {
				runtimeId = info.runtimeId,
				launchNonce = info.launchNonce,
				launchEditRuntimeId = info.launchEditRuntimeId,
				role = info.bridgeRole,
				playerName = info.playerName,
				snapshot = RuntimeApi.finalConsoleSnapshot(),
			}
		end,
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
				editorSync.cleanup()
				if unloading then
					Config.studioChanges.stop()
				end
				table.clear(transactionExpectations)
				RuntimeApi.cleanup(runtimeCleanupGeneration)
				Config.creatorApi.cleanup()
			end
		end,
	})
	Config.studioChangeNotificationConnection = Config.studioChanges.onChanged(function()
		Config.updateStatusText()
		local runtimeSettings = Config.getBridgeSettings()
		local pendingCount = Config.studioChanges.pendingChangeCount()
		if runtimeSettings.notifications ~= false and not Config.hasOpenChannel() and pendingCount > 0 then
			local threshold = tonumber(runtimeSettings.changesThreshold) or 5
			local detail = if pendingCount > threshold
				then `{pendingCount} edits are waiting, above the review threshold of {threshold}.`
				else if pendingCount == 1
					then "One edit is waiting to sync."
					else `{pendingCount} edits are waiting to sync.`
			ui.notify("disconnected-dirty", "Studio changes are waiting", detail, "Connect", Config.connectAll, true)
		else
			ui.dismissNotification("disconnected-dirty")
		end
	end)
end

return BridgePluginRuntime
