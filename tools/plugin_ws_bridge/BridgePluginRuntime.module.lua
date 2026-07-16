--!nocheck

type ServiceState = {
	instances: { Instance },
	classNames: { string },
	classIdByName: { [string]: number },
	generatedAtUnix: number,
	rootName: string,
	rootClassName: string,
	rootPath: string,
	pathByInstance: { [Instance]: string },
	pathSegmentsByInstance: { [Instance]: { string } },
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
	transportDefaultProperties: { [string]: any }?,
	serializedInstances: { [number]: any }?,
	serializedCompactInstances: { [number]: any }?,
	compactWarmStatus: string,
	compactDemandCount: number,
	activeInstanceBatchRequests: number,
	scriptPathsEncoded: string?,
	batchCacheByKey: { [string]: string },
	batchCacheKeys: { string },
	sourceBatchCacheByKey: { [string]: string },
	sourceBatchCacheKeys: { string },
	servicePropertyCandidatesByClass: { [string]: { string } }?,
	servicePropertySchemaByClass: { [string]: { { any } } }?,
	hotPropertySchemaByClass: { [string]: { [string]: any } }?,
	safeReadByClass: { [string]: boolean },
	nameByIndex: { [number]: string },
	classNameByIndex: { [number]: string },
	classValueByIndex: { [number]: any },
	parentIndexByIndex: { [number]: number | boolean },
	requiresPcallByClassProperty: { [string]: { [string]: boolean } },
	modifiedDefaultCheckCount: number,
	modifiedDefaultElidedCount: number,
	modifiedDefaultElidedByClass: { [string]: number },
	modifiedDefaultValidationSamplesByKey: { [string]: number },
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
	
	if not plugin then
		error("Renium must run as a Studio plugin")
	end
	
	local Config = {}
	
	function Config.isPlayModeActiveForBridge(): boolean
		local okState, state = pcall(function()
			return RunService.RunState
		end)
		if okState and state == Enum.RunState.Stopped then
			return false
		end
		local okRunning, running = pcall(function()
			return RunService:IsRunning()
		end)
		if okRunning and running == true then
			return true
		end
		local okEdit, isEdit = pcall(function()
			return RunService:IsEdit()
		end)
		if okEdit and isEdit == false then
			return true
		end
		local okStudioTest, editModeActive = pcall(function()
			return (game:GetService("StudioTestService") :: any).EditModeActive
		end)
		return okStudioTest and editModeActive == false
	end
	
	function Config.getBridgeRole(): string
		local okRunning, running = pcall(function()
			return RunService:IsRunning()
		end)
		if okRunning and running == true then
			local okClient, isClient = pcall(function()
				return RunService:IsClient()
			end)
			if okClient and isClient == true then
				return "play-client"
			end
			return "play-server"
		end
		return "edit"
	end
	
	Config.startedInPlayMode = Config.isPlayModeActiveForBridge()
	Config.bridgeRole = Config.getBridgeRole()

	function Config.getPlayerIdentity(): (string?, number?)
		if Config.bridgeRole ~= "play-client" then
			return nil, nil
		end
		local ok, localPlayer = pcall(function()
			return game:GetService("Players").LocalPlayer
		end)
		if not ok or localPlayer == nil then
			return nil, nil
		end
		return localPlayer.Name, localPlayer.UserId
	end
	
	local SETTINGS_PREFIX = "Renium."
	local DEFAULT_HOST = "127.0.0.1"
	local DEFAULT_PORTS = { 8781, 8782, 8783 }
	local RECONNECT_SECONDS = 0.5
	local FAST_RECONNECT_SECONDS = 0.25
	local FAST_RECONNECT_WINDOW_SECONDS = 8.0
	local CONNECT_OPEN_TIMEOUT_SECONDS = 1.5
	local FAST_CONNECT_OPEN_TIMEOUT_SECONDS = 0.75
	local CONNECT_SESSION_TIMEOUT_SECONDS = 2.0
	local NEXT_RUN_CLOSE_DELAY_SECONDS = 0.02
	local NEXT_RUN_RECONNECT_DELAY_SECONDS = 0.02
	local NEXT_RUN_CONNECT_TIMEOUT_SECONDS = 1.5
	local NEXT_RUN_FAST_WINDOW_SECONDS = 5.0
	local DEBUG_BRIDGE_CONNECTION = false
	local SERIALIZATION_BURST_BUDGET_SECONDS = 1 / 240
	local SERIALIZATION_BURST_CHECK_INTERVAL = 64
	local DEMAND_SERIALIZATION_BURST_BUDGET_SECONDS = 1 / 180
	local DEMAND_SERIALIZATION_BURST_CHECK_INTERVAL = 128
	local BALANCED_DEMAND_SERIALIZATION_BURST_BUDGET_SECONDS = 1 / 240
	local BALANCED_DEMAND_SERIALIZATION_BURST_CHECK_INTERVAL = 256
	local PARALLEL_PRE_SERIALIZE_MIN_ITEMS = 2048
	local PARALLEL_INSTANCE_BATCH_MIN_ITEMS = 768
	local PARALLEL_SOURCE_BATCH_MIN_ITEMS = 24
	local PRE_SERIALIZE_MAX_INSTANCES = 5000
	local PRE_SERIALIZE_WARM_MIN_INSTANCES = 4000
	local PRE_SERIALIZE_WARM_MAX_WORKERS = 4
	local BRIDGE_VERSION = "0.1.2"
	local BRIDGE_PROTOCOL_VERSION = "compact-v5"
	local CODEC_VERSION = "compact-v5-schema-7"
	local BRIDGE_BUILD_UNIX = 1783875358
	local CHUNK_FRAME_PROTOCOL_VERSION = "rbs1"
	local COMPACT_VALUE_PROTOCOL_VERSION = "compact-v5-schema-4"
	local SERIALIZER_WORKER_MODE = "external-preferred-demand-semaphore"
	local LAG_FRAME_MS = 33.3
	local FAST_WARM_FRAME_MS = 20.0
	local CLEAN_DEMAND_SERIALIZER_MAX_FRAME_MS = 33.0
	local THROTTLED_DEMAND_SERIALIZER_MAX_FRAME_MS = 100.0
	local DEFAULT_ACTIVE_DEMAND_SERIALIZERS = 2
	local MAX_ACTIVE_DEMAND_SERIALIZERS = 4
	local MAX_INSTANCE_BATCH_ITEMS = 5000
	local MAX_SOURCE_BATCH_PATHS = 1024
	local MAX_SOURCE_KEY_BYTES = 4096
	local DEFAULT_PERFORMANCE_MODE = "throughput"
	local MODIFIED_DEFAULT_BYPASS_ENABLED = plugin:GetSetting(SETTINGS_PREFIX .. "modifiedDefaultBypass")
	if type(MODIFIED_DEFAULT_BYPASS_ENABLED) ~= "boolean" then
		MODIFIED_DEFAULT_BYPASS_ENABLED = false
	end
	local SHAPE_COMPACT_INSTANCE_BATCHES = plugin:GetSetting(SETTINGS_PREFIX .. "shapeCompactInstanceBatches")
	if type(SHAPE_COMPACT_INSTANCE_BATCHES) ~= "boolean" then
		SHAPE_COMPACT_INSTANCE_BATCHES = true
	end
	local SHAPE_COMPACT_MIN_ITEMS = 128
	local SHAPE_COMPACT_MIN_CELL_SAVINGS = 32
	local COMPACT_SLOT_IN_PROGRESS = {}
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
	local COMPACT_VALUE_TAGS = {
		Vector2 = 1,
		Vector3 = 2,
		UDim = 3,
		UDim2 = 4,
		Color3 = 5,
		BrickColor = 6,
		CFrame = 7,
		Rect = 8,
		EnumItem = 9,
		Font = 10,
		Ref = 11,
		ColorSequence = 12,
		NumberSequence = 13,
		NumberRange = 14,
		PhysicalProperties = 15,
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
	local function requireChildModule(name: string): any
		local child = rootScript:FindFirstChild(name)
		if child and child:IsA("ModuleScript") then
			local ok, result = pcall(require, child)
			if ok and type(result) == "table" then
				return result
			end
			error("[Renium] failed to require module " .. name .. ": " .. tostring(result))
		end
		error("[Renium] missing child ModuleScript: " .. name)
	end
	
	local function tryRequireChildModule(name: string): any?
		local child = rootScript:FindFirstChild(name)
		if child and child:IsA("ModuleScript") then
			local ok, result = pcall(require, child)
			if ok and type(result) == "table" then
				return result
			end
			warn("[Renium] failed optional module " .. name .. ": " .. tostring(result))
		end
		return nil
	end
	
	local function tryRequireNestedModule(parent: Instance?, name: string): any?
		if parent == nil then
			return nil
		end
		local child = parent:FindFirstChild(name)
		if child and child:IsA("ModuleScript") then
			local ok, result = pcall(require, child)
			if ok and type(result) == "table" then
				return result
			end
			warn("[Renium] failed optional nested module " .. name .. ": " .. tostring(result))
		end
		return nil
	end
	
	local SettingsModule = requireChildModule("BridgeSettings")
	local ThemeModule = requireChildModule("BridgeTheme")
	local StatusModule = requireChildModule("BridgeStatus")
	local ParallelModule = requireChildModule("BridgeParallel")
	local ChunkingModule = requireChildModule("BridgeChunking")
	local TransportModule = requireChildModule("BridgeTransport")
	local ConnectionModule = requireChildModule("BridgeConnection")
	local IdentityModule = requireChildModule("BridgeIdentity")
	local UiModule = requireChildModule("BridgeUi")
	local PropertySchemaModule = requireChildModule("BridgePropertySchema")
	local StudioApiSchemaModule = tryRequireChildModule("BridgeStudioApiSchema")
	local EditorSyncModule = requireChildModule("BridgeEditorSync")
	local ProfilingModule = requireChildModule("BridgeProfiling")
	local RuntimeApi = requireChildModule("BridgeRuntimeApi").create(plugin)
	local _ = tryRequireChildModule("RbxDom")
	local RbxDomDatabase = tryRequireNestedModule(rootScript:FindFirstChild("RbxDom"), "database")
	
	local ui = UiModule.create(plugin, ThemeModule, {
		version = BRIDGE_VERSION,
		buildUnix = BRIDGE_BUILD_UNIX,
		codecVersion = CODEC_VERSION,
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
	}
	Config.studioChanges = requireChildModule("BridgeStudioChanges").create(Config, ALLOWED_SERVICES)
	function Config.applyBridgeRuntimeSettings(runtimeSettings: { [string]: any })
		if type(runtimeSettings) ~= "table" then
			return
		end
		if Config.studioChanges ~= nil and type(Config.studioChanges.setOptions) == "function" then
			Config.studioChanges.setOptions({
				syncbackProperties = runtimeSettings.syncbackProperties,
				onlyCodeMode = runtimeSettings.onlyCodeMode,
			})
		end
	end
	do
		local storedConflictResolution =
			SettingsModule.loadConflictResolution(plugin, SETTINGS_PREFIX, nil)
		if storedConflictResolution ~= nil then
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
	local configuredPreSerialize = plugin:GetSetting(SETTINGS_PREFIX .. "preSerialize")
	local PRE_SERIALIZE_ON_PREPARE = configuredPreSerialize == true
	local configuredPreSerializeLargeServiceWarm = plugin:GetSetting(SETTINGS_PREFIX .. "preSerializeLargeServiceWarm")
	local PRE_SERIALIZE_LARGE_SERVICE_WARM = configuredPreSerializeLargeServiceWarm == true
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
		end
	
		if PERFORMANCE_MODE == "balanced" then
			local interval = math.max(1, checkInterval or SERIALIZATION_BURST_CHECK_INTERVAL)
			local budget = math.max(budgetSeconds or SERIALIZATION_BURST_BUDGET_SECONDS, 1 / 120)
			return ParallelModule.makeBurstYielder(interval, budget)
		end
	
		return ParallelModule.makeBurstYielder(checkInterval, budgetSeconds)
	end
	
	local EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS: { [string]: { { any } } } =
		PropertySchemaModule.buildSchemasFromRbxDom(RbxDomDatabase, COMPACT_TYPE_IDS, StudioApiSchemaModule)
	local EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS: { [string]: { string } } = PropertySchemaModule.buildCandidatesFromSchemas(EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS)
	do
		local classCount, propertyCount = PropertySchemaModule.countCandidates(EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS)
		if classCount > 0 then
			print(
				("[Renium] loaded bundled rbx-dom property candidates: classes=%d, properties=%d"):format(
					classCount,
					propertyCount
				)
			)
		end
	end
	
	local function configureStudioChangePropertyCandidates()
		local studioChanges = Config.studioChanges
		if type(studioChanges) ~= "table" then
			return
		end
		local configure = studioChanges.configurePropertyCandidates
		if type(configure) == "function" then
			configure(EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS)
		end
	end
	
	configureStudioChangePropertyCandidates()
	
	local stateByService: { [string]: ServiceState }
	local demandSerializerGate = Instance.new("BindableEvent")
	local activeDemandSerializers = 0
	stateByService = {}
	
	Config.bridgeConnectRequested = false
	Config.bridgeConnectedOnce = false
	Config.bridgeConnectSession = 0
	Config.bridgeConnectDeadline = 0
	Config.bridgeConnectionStatus = "Disconnected"
	Config.bridgePausedForPlay = false
	
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
	
	local editorSync
	
	local serializeRefValueCompactV4
	local serializeValueCompactV4
	local getClassPropertySchema
	local encodeSchemaComparableValue
	local propertyKey
	local serializeAttributesCompactV5
	local prepareService
	local getState
	
	function Config.updateStatusText()
		local statusState = {
			bridgeVersion = BRIDGE_VERSION,
			bridgeBuildUnix = BRIDGE_BUILD_UNIX,
			codecVersion = CODEC_VERSION,
			host = Config.bridgeHost,
			ports = Config.bridgePorts,
			exportAllProperties = EXPORT_ALL_PROPERTIES,
			preSerializeOnPrepare = PRE_SERIALIZE_ON_PREPARE,
			connectionStatus = Config.bridgeConnectionStatus,
			connectRequested = Config.bridgeConnectRequested,
			channels = Config.bridgeChannels,
			editorSyncStats = editorSyncStats,
			bridgeRole = Config.bridgeRole,
		}
		if ui.updateStatus ~= nil then
			ui.updateStatus(StatusModule.view(statusState))
		else
			ui.statusLabel.Text = StatusModule.render(statusState)
		end
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
		invalidateService = function(serviceName: string)
			stateByService[serviceName] = nil
		end,
		updateStatus = Config.updateStatusText,
		getSyncOptions = function()
			return Config.getBridgeSettings and Config.getBridgeSettings() or {}
		end,
	})
	
	local function tryReadModelPivotProperty(instance: Instance, propertyName: string): (boolean, any)
		if not (instance:IsA("Model") or instance:IsA("WorldModel")) then
			return false, nil
		end
		if propertyName == "Scale" then
			return pcall(function()
				return (instance :: any):GetScale()
			end)
		end
		if propertyName == "WorldPivotData" or propertyName == "WorldPivot" or propertyName == "Origin" then
			return pcall(function()
				return (instance :: any):GetPivot()
			end)
		end
		return false, nil
	end
	
	local function tryRead(instance: Instance, propertyName: string): (boolean, any)
		local okModelPivot, modelPivotValue = tryReadModelPivotProperty(instance, propertyName)
		if okModelPivot then
			return true, modelPivotValue
		end
		return pcall(function()
			return (instance :: any)[propertyName]
		end)
	end

	local function physicalPropertiesComparable(value: any): { number }?
		if typeof(value) ~= "PhysicalProperties" then
			return nil
		end
		local acousticAbsorption = 1
		local okAcoustic, rawAcoustic = pcall(function()
			return (value :: any).AcousticAbsorption
		end)
		if okAcoustic and type(rawAcoustic) == "number" then
			acousticAbsorption = rawAcoustic
		end
		return {
			(value :: any).Density,
			(value :: any).Friction,
			(value :: any).Elasticity,
			(value :: any).FrictionWeight,
			(value :: any).ElasticityWeight,
			acousticAbsorption,
		}
	end

	local function physicalPropertiesObject(value: any): any?
		local comparable = physicalPropertiesComparable(value)
		if comparable == nil then
			return nil
		end
		return {
			_type = "PhysicalProperties",
			customPhysics = true,
			density = comparable[1],
			friction = comparable[2],
			elasticity = comparable[3],
			frictionWeight = comparable[4],
			elasticityWeight = comparable[5],
			acousticAbsorption = comparable[6],
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
			local okIsBasePart, isBasePart = pcall(function()
				return instance:IsA("BasePart")
			end)
			if okIsBasePart and isBasePart == true then
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
		if state.modifiedDefaultRuntimeDenylist[bypassKey] ~= true then
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
		local ok, modified = pcall(function()
			return instance:IsPropertyModified(propertyName)
		end)
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
		if MODIFIED_DEFAULT_BYPASS_PROPERTY_DENYLIST[key] == true then
			return false
		end
		if key == "rotation" and className == "Texture" then
			return false
		end
		if typeId == COMPACT_TYPE_IDS.Ref then
			return false
		end
		return true
	end
	
	function Config.evaluateModifiedDefaultBypass(
		state: ServiceState,
		bypassKey: string,
		instance: Instance,
		propertyName: string,
		typeId: number,
		enumType: string?,
		defaultComparable: any,
		compareFn: any
	): (boolean, boolean?, boolean?, boolean, boolean, boolean)
		if state.modifiedDefaultRuntimeDenylist[bypassKey] == true then
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
		local sampleIsDefault = false
		if gotSample and sampledValue ~= nil then
			if compareFn ~= false then
				sampleIsDefault = compareFn(sampledValue, defaultComparable, state)
			else
				sampleIsDefault =
					Config.valueMatchesComparableDefault(typeId, enumType, sampledValue, defaultComparable, state)
			end
		end
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
		if isModified == false then
			stats.unmodified += 1
		end
	
		if isModified == false and not sampleIsDefault then
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
	
	local function contentSourceToUri(value: any): string
		if value.SourceType == Enum.ContentSourceType.Uri then
			return value.Uri or ""
		end
		return ""
	end

	local function serializeValue(value: any, state: ServiceState?): any
		local valueType = typeof(value)
		if valueType == "number" or valueType == "string" or valueType == "boolean" then
			return value
		elseif valueType == "Vector2" then
			return { _type = "Vector2", x = value.X, y = value.Y }
		elseif valueType == "Vector3" then
			return { _type = "Vector3", x = value.X, y = value.Y, z = value.Z }
		elseif valueType == "UDim" then
			return { _type = "UDim", scale = value.Scale, offset = value.Offset }
		elseif valueType == "UDim2" then
			return {
				_type = "UDim2",
				xScale = value.X.Scale,
				xOffset = value.X.Offset,
				yScale = value.Y.Scale,
				yOffset = value.Y.Offset,
			}
		elseif valueType == "Color3" then
			return { _type = "Color3", r = value.R, g = value.G, b = value.B }
		elseif valueType == "BrickColor" then
			return { _type = "BrickColor", number = value.Number }
		elseif valueType == "NumberRange" then
			return { _type = "NumberRange", min = value.Min, max = value.Max }
		elseif valueType == "PhysicalProperties" then
			return physicalPropertiesObject(value)
		elseif valueType == "ColorSequence" then
			local keypoints = {}
			for i, keypoint in ipairs(value.Keypoints) do
				keypoints[i] = { time = keypoint.Time, value = { r = keypoint.Value.R, g = keypoint.Value.G, b = keypoint.Value.B } }
			end
			return { _type = "ColorSequence", keypoints = keypoints }
		elseif valueType == "NumberSequence" then
			local keypoints = {}
			for i, keypoint in ipairs(value.Keypoints) do
				keypoints[i] = { time = keypoint.Time, value = keypoint.Value, envelope = keypoint.Envelope }
			end
			return { _type = "NumberSequence", keypoints = keypoints }
		elseif valueType == "CFrame" then
			return { _type = "CFrame", components = { value:GetComponents() } }
		elseif valueType == "Rect" then
			return { _type = "Rect", minX = value.Min.X, minY = value.Min.Y, maxX = value.Max.X, maxY = value.Max.Y }
		elseif valueType == "EnumItem" then
			return { _type = "EnumItem", enumType = tostring(value.EnumType), name = value.Name }
		elseif valueType == "Font" then
			return { _type = "Font", family = value.Family, weight = tostring(value.Weight), style = tostring(value.Style) }
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
			return {
				_type = "Ray",
				origin = { x = value.Origin.X, y = value.Origin.Y, z = value.Origin.Z },
				direction = { x = value.Direction.X, y = value.Direction.Y, z = value.Direction.Z },
			}
		elseif valueType == "Content" then
			return contentSourceToUri(value)
		elseif valueType == "Instance" then
			return IdentityModule.serializeRefValue(state, value)
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
		if TRANSIENT_TRANSPORT_PROPERTIES[key] == true then
			return true
		end
		if key == "source"
			or key == "robloxlocked"
			or key == "name"
			or key == "classname"
			or key == "parent"
		then
			return true
		end
		if key == "runcontext" and className ~= "Script" then
			return true
		end
		return false
	end
	
	local function isAlwaysDefaultSerialized(propertyName: string, serialized: any): boolean
		if propertyName == "Archivable" and serialized == true then
			return true
		end
		if propertyName == "LinkedSource" and serialized == "" then
			return true
		end
		return false
	end
	
	local function getDefaultSerializedProperties(className: string): any
		local cached = DEFAULT_PROPERTY_CACHE[className]
		if cached ~= nil then
			if cached == NO_DEFAULTS then
				return nil
			end
			return cached
		end
	
		local ok, probe = pcall(function()
			return Instance.new(className)
		end)
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
	
		local ok, probe = pcall(function()
			return Instance.new(className)
		end)
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
				if normalized == "" or shouldSkipStructuralTransportProperty(className, normalized) then
					return nil
				end
				return { normalized, COMPACT_TYPE_IDS.String, false }
			end
			if type(rawEntry) ~= "table" then
				return nil
			end
			local rawName = rawEntry[1]
			local rawTypeId = rawEntry[2]
			if type(rawName) ~= "string" or type(rawTypeId) ~= "number" then
				return nil
			end
			local normalized = normalizePropertyName(rawName)
			if normalized == "" or shouldSkipStructuralTransportProperty(className, normalized) then
				return nil
			end
			local enumType = rawEntry[3]
			if type(enumType) ~= "string" or enumType == "" then
				enumType = false
			end
			return { normalized, rawTypeId, enumType }
		end
	
		EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS = {}
		EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS = {}
		DEFAULT_PROPERTY_CACHE = {}
		DEFAULT_TRANSPORT_PROPERTY_CACHE = {}
		DEFAULT_TRANSPORT_FAST_COMPARE_CACHE = {}
		CLASS_PROPERTY_CANDIDATES_CACHE = {}
		CLASS_PROPERTY_SCHEMA_CACHE = {}
		for serviceName, _ in pairs(stateByService) do
			stateByService[serviceName] = nil
		end
	
		local classCount = 0
		local propertyCount = 0
		for className, names in pairs(payload) do
			if type(className) == "string" and type(names) == "table" then
				local sanitizedSchema = {}
				local sanitized = {}
				local seen: { [string]: boolean } = {}
				for _, rawEntry in ipairs(names) do
					local schemaEntry = sanitizeSchemaEntry(className, rawEntry)
					if schemaEntry ~= nil then
						local normalized = schemaEntry[1]
						local key = propertyKey(normalized)
						if not seen[key] then
							seen[key] = true
							sanitized[#sanitized + 1] = normalized
							sanitizedSchema[#sanitizedSchema + 1] = schemaEntry
						end
					end
				end
				if #sanitized > 0 then
					EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS[className] = sanitizedSchema
					EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS[className] = sanitized
					classCount += 1
					propertyCount += #sanitized
				end
			end
		end
	
		configureStudioChangePropertyCandidates()
	
		return {
			ok = true,
			classCount = classCount,
			propertyCount = propertyCount,
		}
	end
	
	local function configureExportOptions(payload: any): { [string]: any }
		local options = payload
		if type(payload) ~= "table" then
			options = {}
		end
		if options.preSerializeOnPrepare ~= nil then
			PRE_SERIALIZE_ON_PREPARE = options.preSerializeOnPrepare == true
		end
		if options.preSerializeLargeServiceWarm ~= nil then
			PRE_SERIALIZE_LARGE_SERVICE_WARM = options.preSerializeLargeServiceWarm == true
		end
		if options.performanceMode ~= nil then
			PERFORMANCE_MODE = Config.normalizePerformanceMode(options.performanceMode)
		end
		if options.modifiedDefaultBypass ~= nil then
			local previousModifiedDefaultBypass = MODIFIED_DEFAULT_BYPASS_ENABLED
			MODIFIED_DEFAULT_BYPASS_ENABLED = options.modifiedDefaultBypass == true
			if MODIFIED_DEFAULT_BYPASS_ENABLED ~= previousModifiedDefaultBypass then
				for _, serviceState in pairs(stateByService) do
					serviceState.hotPropertySchemaByClass = nil
				end
			end
		end
		if options.exportAllProperties ~= nil then
			local previousExportAllProperties = EXPORT_ALL_PROPERTIES
			EXPORT_ALL_PROPERTIES = options.exportAllProperties == true
			if EXPORT_ALL_PROPERTIES ~= previousExportAllProperties then
				table.clear(CLASS_PROPERTY_SCHEMA_CACHE)
				table.clear(CLASS_PROPERTY_CANDIDATES_CACHE)
				for _, serviceState in pairs(stateByService) do
					serviceState.hotPropertySchemaByClass = nil
					serviceState.serializedInstances = nil
				end
			end
		end
		plugin:SetSetting(SETTINGS_PREFIX .. "exportAllProperties", EXPORT_ALL_PROPERTIES)
		plugin:SetSetting(SETTINGS_PREFIX .. "preSerialize", PRE_SERIALIZE_ON_PREPARE)
		plugin:SetSetting(SETTINGS_PREFIX .. "preSerializeLargeServiceWarm", PRE_SERIALIZE_LARGE_SERVICE_WARM)
		plugin:SetSetting(SETTINGS_PREFIX .. "performanceMode", PERFORMANCE_MODE)
		plugin:SetSetting(SETTINGS_PREFIX .. "modifiedDefaultBypass", MODIFIED_DEFAULT_BYPASS_ENABLED)
		Config.updateStatusText()
		return {
			exportAllProperties = EXPORT_ALL_PROPERTIES,
			preSerializeOnPrepare = PRE_SERIALIZE_ON_PREPARE,
			preSerializeLargeServiceWarm = PRE_SERIALIZE_LARGE_SERVICE_WARM,
			performanceMode = PERFORMANCE_MODE,
			modifiedDefaultBypass = MODIFIED_DEFAULT_BYPASS_ENABLED,
		}
	end
	
	getClassPropertySchema = function(className: string): { { any } }?
		local cached = CLASS_PROPERTY_SCHEMA_CACHE[className]
		if cached == nil then
			local external = EXTERNAL_PROPERTY_SCHEMAS_BY_CLASS[className]
			if external ~= nil and #external > 0 then
				local ok, probe = pcall(function()
					return Instance.new(className)
				end)
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
	
	serializeRefValueCompactV4 = function(state: ServiceState?, instance: Instance): any
		if state ~= nil then
			local instanceIndex = IdentityModule.getCachedInstanceIndex(state, instance)
			if instanceIndex ~= nil then
				return { COMPACT_VALUE_TAGS.Ref, instanceIndex }
			end
	
			local pathSegments = IdentityModule.getCachedRefPathSegments(state, instance)
			if pathSegments == nil or #pathSegments == 0 then
				return nil
			end
	
			local debugId = IdentityModule.getCachedDebugId(state, instance)
			return {
				COMPACT_VALUE_TAGS.Ref,
				false,
				pathSegments,
				debugId or false,
			}
		end
	
		local pathSegments = IdentityModule.getRefPathSegments(instance)
		if pathSegments == nil or #pathSegments == 0 then
			return nil
		end
	
		local debugId = IdentityModule.getDebugId(instance)
		return {
			COMPACT_VALUE_TAGS.Ref,
			false,
			pathSegments,
			debugId or false,
		}
	end
	
	serializeValueCompactV4 = function(value: any, state: ServiceState?): any
		local valueType = typeof(value)
		if valueType == "number" or valueType == "string" or valueType == "boolean" then
			return value
		elseif valueType == "Vector2" then
			return { COMPACT_VALUE_TAGS.Vector2, value.X, value.Y }
		elseif valueType == "Vector3" then
			return { COMPACT_VALUE_TAGS.Vector3, value.X, value.Y, value.Z }
		elseif valueType == "UDim" then
			return { COMPACT_VALUE_TAGS.UDim, value.Scale, value.Offset }
		elseif valueType == "UDim2" then
			return {
				COMPACT_VALUE_TAGS.UDim2,
				value.X.Scale,
				value.X.Offset,
				value.Y.Scale,
				value.Y.Offset,
			}
		elseif valueType == "Color3" then
			return { COMPACT_VALUE_TAGS.Color3, value.R, value.G, value.B }
		elseif valueType == "BrickColor" then
			return { COMPACT_VALUE_TAGS.BrickColor, value.Number }
		elseif valueType == "NumberRange" then
			return { COMPACT_VALUE_TAGS.NumberRange, value.Min, value.Max }
		elseif valueType == "PhysicalProperties" then
			local comparable = physicalPropertiesComparable(value)
			if comparable ~= nil then
				return {
					COMPACT_VALUE_TAGS.PhysicalProperties,
					comparable[1],
					comparable[2],
					comparable[3],
					comparable[4],
					comparable[5],
					comparable[6],
				}
			end
		elseif valueType == "ColorSequence" then
			local out = table.create(#value.Keypoints * 4 + 1)
			out[1] = COMPACT_VALUE_TAGS.ColorSequence
			local writeIndex = 2
			for _, keypoint in ipairs(value.Keypoints) do
				out[writeIndex] = keypoint.Time
				out[writeIndex + 1] = keypoint.Value.R
				out[writeIndex + 2] = keypoint.Value.G
				out[writeIndex + 3] = keypoint.Value.B
				writeIndex += 4
			end
			return out
		elseif valueType == "NumberSequence" then
			local out = table.create(#value.Keypoints * 3 + 1)
			out[1] = COMPACT_VALUE_TAGS.NumberSequence
			local writeIndex = 2
			for _, keypoint in ipairs(value.Keypoints) do
				out[writeIndex] = keypoint.Time
				out[writeIndex + 1] = keypoint.Value
				out[writeIndex + 2] = keypoint.Envelope
				writeIndex += 3
			end
			return out
		elseif valueType == "CFrame" then
			local components = { value:GetComponents() }
			local out = table.create(#components + 1)
			out[1] = COMPACT_VALUE_TAGS.CFrame
			for i, component in ipairs(components) do
				out[i + 1] = component
			end
			return out
		elseif valueType == "Rect" then
			return { COMPACT_VALUE_TAGS.Rect, value.Min.X, value.Min.Y, value.Max.X, value.Max.Y }
		elseif valueType == "EnumItem" then
			return { COMPACT_VALUE_TAGS.EnumItem, tostring(value.EnumType), value.Name }
		elseif valueType == "Font" then
			return { COMPACT_VALUE_TAGS.Font, value.Family, tostring(value.Weight), tostring(value.Style) }
		elseif valueType == "Content" then
			return contentSourceToUri(value)
		elseif valueType == "Instance" then
			return serializeRefValueCompactV4(state, value)
		end
		return nil
	end
	
	local function getOrdinalPathSourceKey(state: ServiceState, instance: Instance): string
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
			table.insert(ordinals, 1, tostring(ordinal))
			current = parent
		end
		return "pathord:" .. table.concat(ordinals, ",") .. ":" .. IdentityModule.getCachedInstancePath(state, instance)
	end
	
	local function ensureScriptIndex(state: ServiceState)
		if state.scriptPaths and state.scriptInstances and state.scriptIndices and state.scriptInstancesByIndex then
			return
		end
		local scriptPaths = table.create(#state.scriptObjects)
		local scriptIndices = table.create(#state.scriptObjects)
		local scriptInstances = {}
		local scriptInstancesByIndex = {}
		local yieldIfNeeded = makeExportBurstYielder()
		for i, inst in ipairs(state.scriptObjects) do
			local sourceKey = IdentityModule.getCachedScriptSourceKey(state, inst)
			local pathSourceKey = "path:" .. IdentityModule.getCachedInstancePath(state, inst)
			local ordinalPathSourceKey = getOrdinalPathSourceKey(state, inst)
			local sourceIndex = IdentityModule.getCachedInstanceIndex(state, inst)
			scriptPaths[i] = sourceKey
			scriptInstances[sourceKey] = inst
			if scriptInstances[pathSourceKey] == nil then
				scriptInstances[pathSourceKey] = inst
			end
			scriptInstances[ordinalPathSourceKey] = inst
			if sourceIndex ~= nil then
				scriptIndices[i] = sourceIndex
				scriptInstancesByIndex[sourceIndex] = inst
			end
			yieldIfNeeded()
		end
		table.sort(scriptPaths)
		table.sort(scriptIndices)
		state.scriptPaths = scriptPaths
		state.scriptIndices = scriptIndices
		state.scriptInstances = scriptInstances
		state.scriptInstancesByIndex = scriptInstancesByIndex
		state.scriptPathsEncoded = nil
	end
	
	local function exportInstanceInternal(
		state: ServiceState,
		instance: Instance,
		forceSafeReads: boolean,
		path: string,
		parentPath: string?,
		debugId: string?,
		parentDebugId: string?,
		instanceId: string?,
		parentInstanceId: string?,
		includePathData: boolean?
	): { [string]: any }
		local entry: { [string]: any } = {
			name = instance.Name,
			className = instance.ClassName,
			parentInstanceId = parentInstanceId,
			attributes = instance:GetAttributes(),
		}
		if includePathData ~= false then
			entry.path = path
			entry.pathSegments = IdentityModule.getCachedRefPathSegments(state, instance)
			entry.parentPath = parentPath
			entry.parentDebugId = parentDebugId
		end
		local properties = {}
		local defaultProperties = nil
		if not EXPORT_ALL_PROPERTIES then
			defaultProperties = getDefaultSerializedProperties(instance.ClassName)
		end
	
		if includePathData ~= false and debugId then
			entry.debugId = debugId
		end
		if instanceId then
			entry.instanceId = instanceId
		end
	
		local fallbackMap = Config.getClassPropertyFallbackMap(state, instance.ClassName)
		local propertyNames = getClassPropertyCandidates(instance.ClassName) or PROPERTY_CANDIDATES
		for _, propertyName in ipairs(propertyNames) do
			if not shouldSkipStructuralTransportProperty(instance.ClassName, propertyName) then
				local defaultSerialized = defaultProperties and defaultProperties[propertyName] or nil
				local skipRead = false
				local hasModifiedState = false
				if not EXPORT_ALL_PROPERTIES then
					local hasModified, isModified = Config.tryIsPropertyModified(instance, propertyName)
					hasModifiedState = hasModified
					if hasModified and not isModified then
						skipRead = true
					end
				end
	
				if not skipRead then
					Config.bumpExportMetric(state, "propertiesRead")
					local value = nil
					local hasValue = false
					if forceSafeReads or fallbackMap[propertyName] == true then
						local got, safeValue = tryRead(instance, propertyName)
						if got then
							value = safeValue
							hasValue = true
						end
					else
						value = (instance :: any)[propertyName]
						hasValue = true
					end
					if propertyName == "CustomPhysicalProperties" then
						hasValue, value = normalizeSchemaTransportValue(COMPACT_TYPE_IDS.PhysicalProperties, propertyName, instance, hasValue, value)
					end
	
					if hasValue and value ~= nil then
						local serialized = serializeValue(value, state)
						if serialized ~= nil then
							if instance.ClassName == "Texture" and propertyName == "Rotation" then
								serialized = nil
							end
						end
						if serialized ~= nil then
							if not EXPORT_ALL_PROPERTIES and not hasModifiedState and isAlwaysDefaultSerialized(propertyName, serialized) then
								serialized = nil
							end
						end
						if serialized ~= nil then
							if EXPORT_ALL_PROPERTIES then
								Config.bumpExportMetric(state, "propertiesEncoded")
								properties[propertyName] = serialized
							elseif defaultSerialized == nil or not deepEqual(serialized, defaultSerialized) then
								Config.bumpExportMetric(state, "propertiesEncoded")
								properties[propertyName] = serialized
							else
								Config.bumpExportMetric(state, "propertiesDefaultSkipped")
							end
						end
					end
				end
			end
		end
	
		if instance:IsA("LuaSourceContainer") then
			properties.Source = "__SOURCE_EXTERNAL__"
			entry.sourceKey = IdentityModule.getCachedScriptSourceKey(state, instance)
		end
		if next(properties) ~= nil then
			entry.properties = properties
		end
		if type(entry.attributes) == "table" and next(entry.attributes) == nil then
			entry.attributes = nil
		end
		return entry
	end
	
	local function exportInstanceFast(
		state: ServiceState,
		instance: Instance,
		path: string,
		parentPath: string?,
		debugId: string?,
		parentDebugId: string?,
		instanceId: string?,
		parentInstanceId: string?,
		includePathData: boolean?
	): { [string]: any }
		return exportInstanceInternal(
			state,
			instance,
			false,
			path,
			parentPath,
			debugId,
			parentDebugId,
			instanceId,
			parentInstanceId,
			includePathData
		)
	end
	
	local function getServicePropertyCandidates(state: ServiceState): { [string]: { string } }
		local cached = state.servicePropertyCandidatesByClass
		if cached ~= nil then
			return cached
		end
	
		local byClass = {}
		for _, className in ipairs(state.classNames) do
			local sourceNames = getClassPropertyCandidates(className) or PROPERTY_CANDIDATES
			local names = table.create(#sourceNames)
			for i, propertyName in ipairs(sourceNames) do
				names[i] = propertyName
			end
			byClass[className] = names
		end
	
		state.servicePropertyCandidatesByClass = byClass
		return byClass
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
		if next(out) == nil then
			local okEnum, liveItems = pcall(function()
				return (Enum :: any)[enumName]:GetEnumItems()
			end)
			if okEnum and type(liveItems) == "table" then
				for _, item in ipairs(liveItems) do
					out[tostring(item.Value)] = item.Name
				end
			end
		end
		if next(out) == nil then
			ENUM_VALUE_NAMES_BY_TYPE_CACHE[enumType] = false
			return false
		end

		ENUM_VALUE_NAMES_BY_TYPE_CACHE[enumType] = out
		return out
	end
	
	local function getServiceEnumValueNamesByType(state: ServiceState): { [string]: any }
		local out = {}
		for _, className in ipairs(state.classNames) do
			local sourceSchema = getClassPropertySchema(className) or {}
			for _, schemaEntry in ipairs(sourceSchema) do
				if schemaEntry[2] == COMPACT_TYPE_IDS.EnumItem and type(schemaEntry[3]) == "string" then
					local enumType = schemaEntry[3]
					if out[enumType] == nil then
						local valueNames = getEnumValueNames(enumType)
						if valueNames ~= false then
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
			local defaultComparable = if defaultProperties ~= nil then defaultProperties[propertyName] else nil
			local defaultFastComparable = if defaultFastCompareProperties ~= nil then defaultFastCompareProperties[propertyName] else defaultComparable
			names[i] = propertyName
			typeIds[i] = typeId
			enumTypes[i] = enumType
			defaults[i] = defaultComparable
			fastDefaults[i] = defaultFastComparable
			canModifiedBypass[i] = Config.canUseModifiedDefaultBypass(className, propertyName, typeId, defaultComparable)
			bypassKeys[i] = if canModifiedBypass[i] then Config.modifiedDefaultBypassKey(className, propertyName) else false
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
			compareFns[i] = Config.compareDefaultValueV5ByTypeId and Config.compareDefaultValueV5ByTypeId[typeId] or false
				encodeFns[i] = Config.encodeValueV5ByTypeId and Config.encodeValueV5ByTypeId[typeId] or false
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
				fastExportSafe = false,
			fallbackExportSafe = false,
			forceSafeReads = false,
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
			if fallbackMap[propertyName] ~= true then
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
	
	local function exportInstanceSafe(
		state: ServiceState,
		instance: Instance,
		path: string,
		parentPath: string?,
		debugId: string?,
		parentDebugId: string?,
		instanceId: string?,
		parentInstanceId: string?,
		includePathData: boolean?
	): { [string]: any }
		return exportInstanceInternal(
			state,
			instance,
			true,
			path,
			parentPath,
			debugId,
			parentDebugId,
			instanceId,
			parentInstanceId,
			includePathData
		)
	end
	
	local function exportInstanceWithFallback(
		state: ServiceState,
		instance: Instance,
		path: string,
		parentPath: string?,
		debugId: string?,
		parentDebugId: string?,
		instanceId: string?,
		parentInstanceId: string?,
		includePathData: boolean?
	): { [string]: any }
		local className = instance.ClassName
		local ok, entry = pcall(
			exportInstanceFast,
			state,
			instance,
			path,
			parentPath,
			debugId,
			parentDebugId,
			instanceId,
			parentInstanceId,
			includePathData
		)
		if ok then
			return entry
		end
	
		local propertyNames = getClassPropertyCandidates(className) or PROPERTY_CANDIDATES
		local learned = Config.learnClassPropertyFallbacks(state, instance, className, propertyNames)
		if learned then
			local retryOk, retryEntry = pcall(
				exportInstanceFast,
				state,
				instance,
				path,
				parentPath,
				debugId,
				parentDebugId,
				instanceId,
				parentInstanceId,
				includePathData
			)
			if retryOk then
				return retryEntry
			end
		end
		return exportInstanceSafe(
			state,
			instance,
			path,
			parentPath,
			debugId,
			parentDebugId,
			instanceId,
			parentInstanceId,
			includePathData
		)
	end
	
	local function exportInstanceWithoutSharedFallback(
		state: ServiceState,
		instance: Instance,
		path: string,
		parentPath: string?,
		debugId: string?,
		parentDebugId: string?,
		instanceId: string?,
		parentInstanceId: string?,
		includePathData: boolean?
	): { [string]: any }
		local className = instance.ClassName
		local safeMode = state.safeReadByClass[className]
		if safeMode == true then
			return exportInstanceSafe(
				state,
				instance,
				path,
				parentPath,
				debugId,
				parentDebugId,
				instanceId,
				parentInstanceId,
				includePathData
			)
		end
		if safeMode == false then
			return exportInstanceFast(
				state,
				instance,
				path,
				parentPath,
				debugId,
				parentDebugId,
				instanceId,
				parentInstanceId,
				includePathData
			)
		end
	
		local ok, entry = pcall(
			exportInstanceFast,
			state,
			instance,
			path,
			parentPath,
			debugId,
			parentDebugId,
			instanceId,
			parentInstanceId,
			includePathData
		)
		if ok then
			state.safeReadByClass[className] = false
			return entry
		end
	
		state.safeReadByClass[className] = true
		return exportInstanceSafe(
			state,
			instance,
			path,
			parentPath,
			debugId,
			parentDebugId,
			instanceId,
			parentInstanceId,
			includePathData
		)
	end
	
	local function compactInstanceEntry(state: ServiceState, entry: { [string]: any }): { any }
		local compactProperties = false
		local properties = entry.properties
		if type(properties) == "table" and next(properties) ~= nil then
			local servicePropertyCandidates = getServicePropertyCandidates(state)
			local propertyNames = servicePropertyCandidates[entry.className or ""] or PROPERTY_CANDIDATES
			local pairsOut = table.create(#propertyNames * 2)
			local emitted = {}
			for i, propertyName in ipairs(propertyNames) do
				local value = properties[propertyName]
				if value ~= nil then
					pairsOut[#pairsOut + 1] = i - 1
					pairsOut[#pairsOut + 1] = value
					emitted[propertyName] = true
				end
			end
	
			local leftovers = {}
			for propertyName, _ in pairs(properties) do
				if propertyName ~= "Source" and emitted[propertyName] ~= true then
					leftovers[#leftovers + 1] = propertyName
				end
			end
			table.sort(leftovers)
			for _, propertyName in ipairs(leftovers) do
				pairsOut[#pairsOut + 1] = propertyName
				pairsOut[#pairsOut + 1] = properties[propertyName]
			end
	
			if #pairsOut > 0 then
				compactProperties = pairsOut
			end
		end
	
		local classValue = IdentityModule.compactClassValue(state, tostring(entry.className or ""))
		local instanceIndex = IdentityModule.parseInstanceIndexId(entry.instanceId)
		local parentIndex = IdentityModule.parseInstanceIndexId(entry.parentInstanceId)
		return {
			entry.name or "",
			classValue,
			compactProperties,
			entry.sourceKey or false,
			entry.attributes or false,
			instanceIndex or false,
			parentIndex or false,
		}
	end
	
	local function serializeAttributesCompactV4(attributes: { [string]: any }, state: ServiceState): any
		if type(attributes) ~= "table" or next(attributes) == nil then
			return false
		end
	
		local out = {}
		local count = 0
		for name, value in pairs(attributes) do
			local serialized = serializeValueCompactV4(value, state)
			if serialized ~= nil then
				out[name] = serialized
				count += 1
			end
		end
	
		if count == 0 then
			return false
		end
		return out
	end
	
	local function markCompactPropertyMask(maskWords: { number }, propertyIndex: number, maskWordCount: number): number
		local zeroBased = propertyIndex - 1
		local wordIndex = math.floor(zeroBased / 32) + 1
		local bitIndex = zeroBased % 32
		for fillIndex = #maskWords + 1, wordIndex do
			if maskWords[fillIndex] == nil then
				maskWords[fillIndex] = 0
			end
		end
		maskWords[wordIndex] = bit32.bor(maskWords[wordIndex] or 0, bit32.lshift(1, bitIndex))
		if wordIndex > maskWordCount then
			return wordIndex
		end
		return maskWordCount
	end
	
	local function exportCompactInstanceEntryInternal(
		state: ServiceState,
		instance: Instance,
		safeReads: boolean,
		classValue: any,
		instanceIndex: number?,
		parentIndex: number?
	): { any }
		local attributes = serializeAttributesCompactV4(instance:GetAttributes(), state)
		local defaultProperties = if not EXPORT_ALL_PROPERTIES then getDefaultTransportProperties(instance.ClassName) else nil
		local propertyNames = getClassPropertyCandidates(instance.ClassName) or PROPERTY_CANDIDATES
		local maskWords = table.create(math.ceil(#propertyNames / 32))
		local maskWordCount = 0
		local valuesOut = table.create(#propertyNames)
		local valueWriteIndex = 0
		for i, propertyName in ipairs(propertyNames) do
			if not shouldSkipStructuralTransportProperty(instance.ClassName, propertyName) then
				local defaultSerialized = defaultProperties and defaultProperties[propertyName] or nil
				local skipRead = false
				local hasModifiedState = false
				if not EXPORT_ALL_PROPERTIES then
					local hasModified, isModified = Config.tryIsPropertyModified(instance, propertyName)
					hasModifiedState = hasModified
					if hasModified and not isModified then
						skipRead = true
					end
				end
	
				if not skipRead then
					local value = nil
					local hasValue = false
					if safeReads then
						local got, safeValue = tryRead(instance, propertyName)
						if got then
							value = safeValue
							hasValue = true
						end
					else
						value = (instance :: any)[propertyName]
						hasValue = true
					end
					if propertyName == "CustomPhysicalProperties" then
						hasValue, value = normalizeSchemaTransportValue(COMPACT_TYPE_IDS.PhysicalProperties, propertyName, instance, hasValue, value)
					end
	
					if hasValue and value ~= nil then
						local serialized = serializeValueCompactV4(value, state)
						if serialized ~= nil and instance.ClassName == "Texture" and propertyName == "Rotation" then
							serialized = nil
						end
						if serialized ~= nil then
							if not EXPORT_ALL_PROPERTIES and not hasModifiedState and isAlwaysDefaultSerialized(propertyName, serialized) then
								serialized = nil
							end
						end
						if
							serialized ~= nil
							and not EXPORT_ALL_PROPERTIES
							and defaultSerialized ~= nil
							and deepEqual(serialized, defaultSerialized)
						then
							serialized = nil
						end
						if serialized ~= nil then
							maskWordCount = markCompactPropertyMask(maskWords, i, maskWordCount)
							valueWriteIndex += 1
							valuesOut[valueWriteIndex] = serialized
						end
					end
				end
			end
		end
	
		local compactMask = false
		if maskWordCount > 0 then
			local denseMask = table.create(maskWordCount)
			for i = 1, maskWordCount do
				denseMask[i] = maskWords[i] or 0
			end
			compactMask = denseMask
		end
	
		local compactValues = false
		if valueWriteIndex > 0 then
			local denseValues = table.create(valueWriteIndex)
			for i = 1, valueWriteIndex do
				denseValues[i] = valuesOut[i]
			end
			compactValues = denseValues
		end
	
		return {
			instance.Name,
			classValue,
			parentIndex or false,
			attributes,
			compactMask,
			compactValues,
		}
	end
	
	local function exportCompactInstanceEntryFast(
		state: ServiceState,
		inst: Instance,
		classValue: any,
		instanceIndex: number?,
		parentIndex: number?
	): { any }
		return exportCompactInstanceEntryInternal(state, inst, false, classValue, instanceIndex, parentIndex)
	end
	
	local function exportCompactInstanceEntrySafe(
		state: ServiceState,
		inst: Instance,
		classValue: any,
		instanceIndex: number?,
		parentIndex: number?
	): { any }
		return exportCompactInstanceEntryInternal(state, inst, true, classValue, instanceIndex, parentIndex)
	end
	
	local function exportCompactInstanceEntry(state: ServiceState, inst: Instance): { any }
		local className = inst.ClassName
		local classValue = IdentityModule.compactClassValue(state, inst.ClassName)
		local instanceIndex = IdentityModule.getCachedInstanceIndex(state, inst)
		local parentIndex = IdentityModule.getCachedParentInstanceIndex(state, inst)
		local safeMode = state.safeReadByClass[className]
		if safeMode == true then
			return exportCompactInstanceEntrySafe(state, inst, classValue, instanceIndex, parentIndex)
		end
		if safeMode == false then
			return exportCompactInstanceEntryFast(state, inst, classValue, instanceIndex, parentIndex)
		end
		local ok, entry = pcall(exportCompactInstanceEntryFast, state, inst, classValue, instanceIndex, parentIndex)
		if ok then
			state.safeReadByClass[className] = false
			return entry
		end
		state.safeReadByClass[className] = true
		return exportCompactInstanceEntrySafe(state, inst, classValue, instanceIndex, parentIndex)
	end
	
	function Config.internBatchString(
		strings: { string },
		stringIds: { [string]: number },
		text: string
	): number
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
		if value == false or value == nil then
			return "f"
		end
		if valueType == "number" then
			return "n:" .. tostring(value)
		end
		if valueType == "string" then
			return "s:" .. value
		end
		if valueType == "table" then
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
		local compactMask = mask
		if compactMask == nil then
			compactMask = false
		end
		local key = compactShapeKeyPart(classValue) .. "|" .. compactShapeKeyPart(compactMask)
		local existing = shapeIds[key]
		if existing ~= nil then
			return existing
		end
		local nextId = #shapes + 1
		shapes[nextId] = { classValue, compactMask }
		shapeIds[key] = nextId
		return nextId
	end
	
	local function compactV5RowHasPropertyMask(row: { any }): boolean
		local field4 = row[4]
		local field5 = row[5]
		local field6 = row[6]
		if field4 == nil or field5 == nil then
			return false
		end
		if field6 == nil then
			local field4Type = type(field4)
			return field4Type == "number" or field4Type == "table"
		end
		return true
	end
	
	local function shapeCompactV5Row(
		row: { any },
		shapes: { any },
		shapeIds: { [string]: number }
	): ({ any }?, boolean)
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
	
		local shapedItems = table.create(count)
		local shapes = {}
		local shapeIds = {}
		local propertyRowCount = 0
		for i = 1, count do
			local shapedRow, rowHasPropertyMask = shapeCompactV5Row(items[i], shapes, shapeIds)
			if shapedRow == nil then
				return nil, nil, 0
			end
			shapedItems[i] = shapedRow
			if rowHasPropertyMask then
				propertyRowCount += 1
			end
		end
	
		local estimatedCellSavings = propertyRowCount - (#shapes * 2)
		if estimatedCellSavings < SHAPE_COMPACT_MIN_CELL_SAVINGS then
			return nil, nil, estimatedCellSavings
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
	
		local pathSegments = if state ~= nil then IdentityModule.getCachedRefPathSegments(state, instance) else IdentityModule.getRefPathSegments(instance)
		if pathSegments == nil or #pathSegments == 0 then
			return nil
		end
	
		local out = table.create(#pathSegments + 2)
		out[1] = 0
		local debugId = if state ~= nil then IdentityModule.getCachedDebugId(state, instance) else IdentityModule.getDebugId(instance)
		out[2] = debugId or false
		for i, segment in ipairs(pathSegments) do
			out[i + 2] = segment
		end
		return out
	end
	
	encodeSchemaComparableValue = function(
		typeId: number,
		_enumType: string?,
		value: any,
		state: ServiceState?
	): any
		if typeId == COMPACT_TYPE_IDS.Bool then
			if type(value) == "boolean" then
				return value
			end
		elseif typeId == COMPACT_TYPE_IDS.Number then
			if type(value) == "number" then
				return value
			end
		elseif typeId == COMPACT_TYPE_IDS.String or typeId == COMPACT_TYPE_IDS.ContentId or typeId == COMPACT_TYPE_IDS.BinaryString then
			if type(value) == "string" then
				return value
			end
		elseif typeId == COMPACT_TYPE_IDS.Vector2 and typeof(value) == "Vector2" then
			return { value.X, value.Y }
		elseif typeId == COMPACT_TYPE_IDS.Vector3 and typeof(value) == "Vector3" then
			return { value.X, value.Y, value.Z }
		elseif typeId == COMPACT_TYPE_IDS.UDim and typeof(value) == "UDim" then
			return { value.Scale, value.Offset }
		elseif typeId == COMPACT_TYPE_IDS.UDim2 and typeof(value) == "UDim2" then
			return { value.X.Scale, value.X.Offset, value.Y.Scale, value.Y.Offset }
		elseif typeId == COMPACT_TYPE_IDS.Color3 and typeof(value) == "Color3" then
			return { value.R, value.G, value.B }
		elseif typeId == COMPACT_TYPE_IDS.BrickColor and typeof(value) == "BrickColor" then
			return value.Number
		elseif typeId == COMPACT_TYPE_IDS.NumberRange and typeof(value) == "NumberRange" then
			return { value.Min, value.Max }
		elseif typeId == COMPACT_TYPE_IDS.PhysicalProperties then
			if value == false then
				return false
			end
			return physicalPropertiesComparable(value)
		elseif typeId == COMPACT_TYPE_IDS.EnumItem and typeof(value) == "EnumItem" then
			return value.Name
		elseif typeId == COMPACT_TYPE_IDS.CFrame and typeof(value) == "CFrame" then
			return { value:GetComponents() }
		elseif typeId == COMPACT_TYPE_IDS.Rect and typeof(value) == "Rect" then
			return { value.Min.X, value.Min.Y, value.Max.X, value.Max.Y }
		elseif typeId == COMPACT_TYPE_IDS.Font and typeof(value) == "Font" then
			return { value.Family, tostring(value.Weight), tostring(value.Style) }
		elseif typeId == COMPACT_TYPE_IDS.ColorSequence and typeof(value) == "ColorSequence" then
			local out = table.create(#value.Keypoints * 4)
			local writeIndex = 1
			for _, keypoint in ipairs(value.Keypoints) do
				out[writeIndex] = keypoint.Time
				out[writeIndex + 1] = keypoint.Value.R
				out[writeIndex + 2] = keypoint.Value.G
				out[writeIndex + 3] = keypoint.Value.B
				writeIndex += 4
			end
			return out
		elseif typeId == COMPACT_TYPE_IDS.NumberSequence and typeof(value) == "NumberSequence" then
			local out = table.create(#value.Keypoints * 3)
			local writeIndex = 1
			for _, keypoint in ipairs(value.Keypoints) do
				out[writeIndex] = keypoint.Time
				out[writeIndex + 1] = keypoint.Value
				out[writeIndex + 2] = keypoint.Envelope
				writeIndex += 3
			end
			return out
		elseif typeId == COMPACT_TYPE_IDS.Axes and typeof(value) == "Axes" then
			return axesBitmask(value)
		elseif typeId == COMPACT_TYPE_IDS.Faces and typeof(value) == "Faces" then
			return facesBitmask(value)
		elseif typeId == COMPACT_TYPE_IDS.Ray and typeof(value) == "Ray" then
			return {
				value.Origin.X,
				value.Origin.Y,
				value.Origin.Z,
				value.Direction.X,
				value.Direction.Y,
				value.Direction.Z,
			}
		elseif typeId == COMPACT_TYPE_IDS.Ref and typeof(value) == "Instance" then
			return encodeComparableRefValue(state, value)
		end

		return nil
	end
	
	function Config.valueMatchesComparableDefault(
		typeId: number,
		_enumType: string?,
		value: any,
		defaultComparable: any,
		state: ServiceState?
	): boolean
		if defaultComparable == nil then
			return false
		end
		if typeId == COMPACT_TYPE_IDS.Bool then
			return type(value) == "boolean" and value == defaultComparable
		elseif typeId == COMPACT_TYPE_IDS.Number then
			return type(value) == "number" and value == defaultComparable
		elseif typeId == COMPACT_TYPE_IDS.String or typeId == COMPACT_TYPE_IDS.ContentId or typeId == COMPACT_TYPE_IDS.BinaryString then
			return type(value) == "string" and value == defaultComparable
		elseif typeId == COMPACT_TYPE_IDS.Vector2 and typeof(value) == "Vector2" then
			return type(defaultComparable) == "table" and value.X == defaultComparable[1] and value.Y == defaultComparable[2]
		elseif typeId == COMPACT_TYPE_IDS.Vector3 and typeof(value) == "Vector3" then
			return type(defaultComparable) == "table"
				and value.X == defaultComparable[1]
				and value.Y == defaultComparable[2]
				and value.Z == defaultComparable[3]
		elseif typeId == COMPACT_TYPE_IDS.UDim and typeof(value) == "UDim" then
			return type(defaultComparable) == "table"
				and value.Scale == defaultComparable[1]
				and value.Offset == defaultComparable[2]
		elseif typeId == COMPACT_TYPE_IDS.UDim2 and typeof(value) == "UDim2" then
			return type(defaultComparable) == "table"
				and value.X.Scale == defaultComparable[1]
				and value.X.Offset == defaultComparable[2]
				and value.Y.Scale == defaultComparable[3]
				and value.Y.Offset == defaultComparable[4]
		elseif typeId == COMPACT_TYPE_IDS.Color3 and typeof(value) == "Color3" then
			return type(defaultComparable) == "table"
				and value.R == defaultComparable[1]
				and value.G == defaultComparable[2]
				and value.B == defaultComparable[3]
		elseif typeId == COMPACT_TYPE_IDS.BrickColor and typeof(value) == "BrickColor" then
			return value.Number == defaultComparable
		elseif typeId == COMPACT_TYPE_IDS.NumberRange and typeof(value) == "NumberRange" then
			return type(defaultComparable) == "table" and value.Min == defaultComparable[1] and value.Max == defaultComparable[2]
		elseif typeId == COMPACT_TYPE_IDS.PhysicalProperties then
			if defaultComparable == false then
				return value == false
			end
			local comparable = physicalPropertiesComparable(value)
			return comparable ~= nil and deepEqual(comparable, defaultComparable)
		elseif typeId == COMPACT_TYPE_IDS.EnumItem and typeof(value) == "EnumItem" then
			return value.Name == defaultComparable
		elseif typeId == COMPACT_TYPE_IDS.CFrame and typeof(value) == "CFrame" then
			if type(defaultComparable) ~= "table" or #defaultComparable ~= 12 then
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
		elseif typeId == COMPACT_TYPE_IDS.Rect and typeof(value) == "Rect" then
			return type(defaultComparable) == "table"
				and value.Min.X == defaultComparable[1]
				and value.Min.Y == defaultComparable[2]
				and value.Max.X == defaultComparable[3]
				and value.Max.Y == defaultComparable[4]
		elseif typeId == COMPACT_TYPE_IDS.Font and typeof(value) == "Font" then
			return type(defaultComparable) == "table"
				and value.Family == defaultComparable[1]
				and tostring(value.Weight) == defaultComparable[2]
				and tostring(value.Style) == defaultComparable[3]
		elseif typeId == COMPACT_TYPE_IDS.ColorSequence and typeof(value) == "ColorSequence" then
			if type(defaultComparable) ~= "table" then
				return false
			end
			local keypoints = value.Keypoints
			if #keypoints * 4 ~= #defaultComparable then
				return false
			end
			local writeIndex = 1
			for _, keypoint in ipairs(keypoints) do
				if keypoint.Time ~= defaultComparable[writeIndex]
					or keypoint.Value.R ~= defaultComparable[writeIndex + 1]
					or keypoint.Value.G ~= defaultComparable[writeIndex + 2]
					or keypoint.Value.B ~= defaultComparable[writeIndex + 3]
				then
					return false
				end
				writeIndex += 4
			end
			return true
		elseif typeId == COMPACT_TYPE_IDS.NumberSequence and typeof(value) == "NumberSequence" then
			if type(defaultComparable) ~= "table" then
				return false
			end
			local keypoints = value.Keypoints
			if #keypoints * 3 ~= #defaultComparable then
				return false
			end
			local writeIndex = 1
			for _, keypoint in ipairs(keypoints) do
				if keypoint.Time ~= defaultComparable[writeIndex]
					or keypoint.Value ~= defaultComparable[writeIndex + 1]
					or keypoint.Envelope ~= defaultComparable[writeIndex + 2]
				then
					return false
				end
				writeIndex += 3
			end
			return true
		elseif typeId == COMPACT_TYPE_IDS.Axes and typeof(value) == "Axes" then
			return axesBitmask(value) == defaultComparable
		elseif typeId == COMPACT_TYPE_IDS.Faces and typeof(value) == "Faces" then
			return facesBitmask(value) == defaultComparable
		elseif typeId == COMPACT_TYPE_IDS.Ray and typeof(value) == "Ray" then
			return type(defaultComparable) == "table"
				and value.Origin.X == defaultComparable[1]
				and value.Origin.Y == defaultComparable[2]
				and value.Origin.Z == defaultComparable[3]
				and value.Direction.X == defaultComparable[4]
				and value.Direction.Y == defaultComparable[5]
				and value.Direction.Z == defaultComparable[6]
		elseif typeId == COMPACT_TYPE_IDS.Ref and typeof(value) == "Instance" then
			local comparable = encodeSchemaComparableValue(typeId, nil, value, state)
			return deepEqual(comparable, defaultComparable)
		end
		return false
	end
	
	Config.encodeNumberV5 = function(value: any): any
		if type(value) ~= "number" then
			return nil
		end
		if value ~= value then
			return { _type = "Float", value = "nan" }
		end
		if value == math.huge then
			return { _type = "Float", value = "inf" }
		end
		if value == -math.huge then
			return { _type = "Float", value = "-inf" }
		end
		return value
	end

	local function encodeSchemaValueV5(
		typeId: number,
		enumType: string?,
		value: any,
		state: ServiceState?,
		strings: { string },
		stringIds: { [string]: number }
	): any
		if typeId == COMPACT_TYPE_IDS.Bool then
			if type(value) == "boolean" then
				return value
			end
		elseif typeId == COMPACT_TYPE_IDS.Number then
			return Config.encodeNumberV5(value)
		elseif typeId == COMPACT_TYPE_IDS.String or typeId == COMPACT_TYPE_IDS.ContentId or typeId == COMPACT_TYPE_IDS.BinaryString then
			if type(value) == "string" then
				return Config.internBatchString(strings, stringIds, value)
			end
		elseif typeId == COMPACT_TYPE_IDS.Vector2 and typeof(value) == "Vector2" then
			return { value.X, value.Y }
		elseif typeId == COMPACT_TYPE_IDS.Vector3 and typeof(value) == "Vector3" then
			return { value.X, value.Y, value.Z }
		elseif typeId == COMPACT_TYPE_IDS.UDim and typeof(value) == "UDim" then
			return { value.Scale, value.Offset }
		elseif typeId == COMPACT_TYPE_IDS.UDim2 and typeof(value) == "UDim2" then
			return { value.X.Scale, value.X.Offset, value.Y.Scale, value.Y.Offset }
		elseif typeId == COMPACT_TYPE_IDS.Color3 and typeof(value) == "Color3" then
			return { value.R, value.G, value.B }
		elseif typeId == COMPACT_TYPE_IDS.BrickColor and typeof(value) == "BrickColor" then
			return value.Number
		elseif typeId == COMPACT_TYPE_IDS.NumberRange and typeof(value) == "NumberRange" then
			return { value.Min, value.Max }
		elseif typeId == COMPACT_TYPE_IDS.PhysicalProperties then
			if value == false then
				return false
			end
			return physicalPropertiesComparable(value)
		elseif typeId == COMPACT_TYPE_IDS.EnumItem and typeof(value) == "EnumItem" then
			return value.Value
		elseif typeId == COMPACT_TYPE_IDS.CFrame and typeof(value) == "CFrame" then
			return { value:GetComponents() }
		elseif typeId == COMPACT_TYPE_IDS.Rect and typeof(value) == "Rect" then
			return { value.Min.X, value.Min.Y, value.Max.X, value.Max.Y }
		elseif typeId == COMPACT_TYPE_IDS.Font and typeof(value) == "Font" then
			return {
				Config.internBatchString(strings, stringIds, value.Family),
				Config.internBatchString(strings, stringIds, tostring(value.Weight)),
				Config.internBatchString(strings, stringIds, tostring(value.Style)),
			}
		elseif typeId == COMPACT_TYPE_IDS.ColorSequence and typeof(value) == "ColorSequence" then
			return encodeSchemaComparableValue(typeId, enumType, value, state)
		elseif typeId == COMPACT_TYPE_IDS.NumberSequence and typeof(value) == "NumberSequence" then
			return encodeSchemaComparableValue(typeId, enumType, value, state)
		elseif typeId == COMPACT_TYPE_IDS.Axes and typeof(value) == "Axes" then
			return axesBitmask(value)
		elseif typeId == COMPACT_TYPE_IDS.Faces and typeof(value) == "Faces" then
			return facesBitmask(value)
		elseif typeId == COMPACT_TYPE_IDS.Ray and typeof(value) == "Ray" then
			return {
				value.Origin.X,
				value.Origin.Y,
				value.Origin.Z,
				value.Direction.X,
				value.Direction.Y,
				value.Direction.Z,
			}
		elseif typeId == COMPACT_TYPE_IDS.Ref and typeof(value) == "Instance" then
			local comparable = encodeComparableRefValue(state, value)
			if comparable == nil then
				return nil
			end
			if type(comparable) == "number" then
				return comparable
			end
			local out = table.create(#comparable)
			out[1] = 0
			out[2] = if type(comparable[2]) == "string" then Config.internBatchString(strings, stringIds, comparable[2]) else false
			for i = 3, #comparable do
				out[i] = Config.internBatchString(strings, stringIds, comparable[i])
			end
			return out
		end
		return nil
	end
	
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
			return type(value) == "string" and value == defaultComparable
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
				if keypoint.Time ~= defaultComparable[writeIndex]
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
				if keypoint.Time ~= defaultComparable[writeIndex]
					or keypoint.Value ~= defaultComparable[writeIndex + 1]
					or keypoint.Envelope ~= defaultComparable[writeIndex + 2]
				then
					return false
				end
				writeIndex += 3
			end
			return true
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
		[COMPACT_TYPE_IDS.String] = function(value: any, _state: ServiceState?, strings: { string }, stringIds: { [string]: number }): any
			if type(value) == "string" then
				return Config.internBatchString(strings, stringIds, value)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.ContentId] = function(value: any, _state: ServiceState?, strings: { string }, stringIds: { [string]: number }): any
			if type(value) == "string" then
				return Config.internBatchString(strings, stringIds, value)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.BinaryString] = function(value: any, _state: ServiceState?, strings: { string }, stringIds: { [string]: number }): any
			if type(value) == "string" then
				return Config.internBatchString(strings, stringIds, value)
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.Vector2] = function(value: any): any
			if typeof(value) == "Vector2" then
				return { value.X, value.Y }
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.Vector3] = function(value: any): any
			if typeof(value) == "Vector3" then
				return { value.X, value.Y, value.Z }
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.UDim] = function(value: any): any
			if typeof(value) == "UDim" then
				return { value.Scale, value.Offset }
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.UDim2] = function(value: any): any
			if typeof(value) == "UDim2" then
				return { value.X.Scale, value.X.Offset, value.Y.Scale, value.Y.Offset }
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.Color3] = function(value: any): any
			if typeof(value) == "Color3" then
				return { value.R, value.G, value.B }
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
				return { value.Min, value.Max }
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.PhysicalProperties] = function(value: any): any
			if value == false then
				return false
			end
			return physicalPropertiesComparable(value)
		end,
		[COMPACT_TYPE_IDS.EnumItem] = function(value: any, _state: ServiceState?, strings: { string }, stringIds: { [string]: number }): any
			if typeof(value) == "EnumItem" then
				return value.Value
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.CFrame] = function(value: any): any
			if typeof(value) == "CFrame" then
				local c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11 = value:GetComponents()
				return { c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11 }
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.Rect] = function(value: any): any
			if typeof(value) == "Rect" then
				return { value.Min.X, value.Min.Y, value.Max.X, value.Max.Y }
			end
			return nil
		end,
		[COMPACT_TYPE_IDS.Font] = function(value: any, _state: ServiceState?, strings: { string }, stringIds: { [string]: number }): any
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
				out[writeIndex] = keypoint.Time
				out[writeIndex + 1] = keypoint.Value.R
				out[writeIndex + 2] = keypoint.Value.G
				out[writeIndex + 3] = keypoint.Value.B
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
				out[writeIndex] = keypoint.Time
				out[writeIndex + 1] = keypoint.Value
				out[writeIndex + 2] = keypoint.Envelope
				writeIndex += 3
			end
			return out
		end,
		[COMPACT_TYPE_IDS.Ref] = function(value: any, state: ServiceState?, strings: { string }, stringIds: { [string]: number }): any
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
			out[2] = if type(comparable[2]) == "string" then Config.internBatchString(strings, stringIds, comparable[2]) else false
			for i = 3, #comparable do
				out[i] = Config.internBatchString(strings, stringIds, comparable[i])
			end
			return out
		end,
	}
	
	function Config.buildCompactV5Exporter(className: string, hotSchema: { [string]: any }, useFallbackMap: boolean?)
		local propertyCount = hotSchema.count
		local propertyNames = hotSchema.names
		local typeIds = hotSchema.typeIds
		local enumTypes = hotSchema.enumTypes
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
		local shouldUseFallbackMap = useFallbackMap == true
		local exportAllProperties = EXPORT_ALL_PROPERTIES
		local modifiedDefaultBypassEnabled = MODIFIED_DEFAULT_BYPASS_ENABLED and not exportAllProperties
		local evaluateModifiedDefaultBypass = Config.evaluateModifiedDefaultBypass
		local tryIsPropertyModified = Config.tryIsPropertyModified
		local valueMatchesComparableDefault = Config.valueMatchesComparableDefault
		local encodeValue = encodeSchemaValueV5
		local getFallbackMap = Config.getClassPropertyFallbackMap
		local internBatchString = Config.internBatchString
	
		if not modifiedDefaultBypassEnabled then
			return function(
				state: ServiceState,
				instance: Instance,
				instanceIndex: number,
				forceSafeReads: boolean,
				strings: { string },
				stringIds: { [string]: number }
			)
				local classValue = state.classValueByIndex[instanceIndex] or IdentityModule.compactClassValue(state, className)
				local parentIndex = state.parentIndexByIndex[instanceIndex]
				local attributes = serializeAttributesCompactV5(instance:GetAttributes(), state, strings, stringIds)
				local fallbackMap = nil
				if shouldUseFallbackMap then
					fallbackMap = getFallbackMap(state, className)
				end
				local maskWords = nil
				local maskWordCount = 0
				local valuesOut = nil
				local valueWriteIndex = 0
	
				for i = 1, propertyCount do
					local propertyName = propertyNames[i]
					local value = nil
					local hasValue = false
					if forceSafeReads or (fallbackMap ~= nil and fallbackMap[propertyName] == true) then
						local got, safeValue = tryRead(instance, propertyName)
						if got then
							value = safeValue
							hasValue = true
						end
					else
						value = (instance :: any)[propertyName]
						hasValue = true
					end
					if propertyName == "CustomPhysicalProperties" then
						hasValue, value = normalizeSchemaTransportValue(typeIds[i], propertyName, instance, hasValue, value)
					end
	
					if hasValue and value ~= nil then
						local isDefault = false
						if not exportAllProperties then
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
								isDefault = value.Scale == defaultFastComparable[1] and value.Offset == defaultFastComparable[2]
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
								if compareFn ~= false then
									isDefault = compareFn(value, defaultComparable, state)
								else
									isDefault = valueMatchesComparableDefault(typeIds[i], enumTypes[i], value, defaultComparable, state)
								end
							end
						end
						if not isDefault then
							if skipEncode[i] ~= true then
								local encodeFn = encodeFns[i]
								local encoded = nil
								if encodeFn ~= false then
									encoded = encodeFn(value, state, strings, stringIds, enumTypes[i])
								else
									encoded = encodeValue(typeIds[i], enumTypes[i], value, state, strings, stringIds)
								end
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
	
				local compactValues = false
				if valuesOut ~= nil and valueWriteIndex > 0 then
					compactValues = valuesOut
				end
	
				local nameId = internBatchString(strings, stringIds, state.nameByIndex[instanceIndex] or instance.Name)
				if compactMask == false and compactValues == false then
					if attributes == false then
						return {
							nameId,
							classValue,
							parentIndex or false,
						}, 0, 0, 0, 0, 0, 0, 0
					end
					return {
						nameId,
						classValue,
						parentIndex or false,
						attributes,
					}, 0, 0, 0, 0, 0, 0, 0
				end
	
				if attributes == false then
					return {
						nameId,
						classValue,
						parentIndex or false,
						compactMask,
						compactValues,
					}, 0, 0, 0, 0, 0, 0, 0
				end
	
				return {
					nameId,
					classValue,
					parentIndex or false,
					attributes,
					compactMask,
					compactValues,
				}, 0, 0, 0, 0, 0, 0, 0
			end
		end
	
		return function(
			state: ServiceState,
			instance: Instance,
			instanceIndex: number,
			forceSafeReads: boolean,
			strings: { string },
			stringIds: { [string]: number }
		)
			local classValue = state.classValueByIndex[instanceIndex] or IdentityModule.compactClassValue(state, className)
			local parentIndex = state.parentIndexByIndex[instanceIndex]
			local attributes = serializeAttributesCompactV5(instance:GetAttributes(), state, strings, stringIds)
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
				local skipRead = false
				if modifiedDefaultBypassEnabled then
					local bypassKey = bypassKeys[i]
					if bypassKey ~= false and state.modifiedDefaultRuntimeDenylist[bypassKey] ~= true and canModifiedBypass[i] then
					local shouldUseBypass, sampledHasModified, sampledIsModified, sampledCheck, sampledValidationRead, sampledDenylist =
						evaluateModifiedDefaultBypass(
							state,
							bypassKey,
							instance,
							propertyName,
							typeIds[i],
							enumTypes[i],
							defaultComparable,
							compareFns[i]
						)
					if sampledCheck then
						state.modifiedDefaultCheckCount += 1
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
						state.modifiedDefaultCheckCount += 1
						modifiedDefaultChecks += 1
						hasModified, isModified = tryIsPropertyModified(instance, propertyName)
					end
					if shouldUseBypass and hasModified and not isModified then
						skipRead = true
						if skipRead then
							state.modifiedDefaultElidedCount += 1
							state.modifiedDefaultElidedByClass[className] =
								(state.modifiedDefaultElidedByClass[className] or 0) + 1
							modifiedDefaultElided += 1
						end
					end
					end
				end
	
				if not skipRead then
					propertiesRead += 1
					local value = nil
					local hasValue = false
					if forceSafeReads or (fallbackMap ~= nil and fallbackMap[propertyName] == true) then
						local got, safeValue = tryRead(instance, propertyName)
						if got then
							value = safeValue
							hasValue = true
						end
					else
						value = (instance :: any)[propertyName]
						hasValue = true
					end
					if propertyName == "CustomPhysicalProperties" then
						hasValue, value = normalizeSchemaTransportValue(typeIds[i], propertyName, instance, hasValue, value)
					end
	
					if hasValue and value ~= nil then
						local isDefault = false
						if not exportAllProperties then
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
								isDefault = value.Scale == defaultFastComparable[1] and value.Offset == defaultFastComparable[2]
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
								if compareFn ~= false then
									isDefault = compareFn(value, defaultComparable, state)
								else
									isDefault = valueMatchesComparableDefault(typeIds[i], enumTypes[i], value, defaultComparable, state)
								end
							end
						end
						if not isDefault then
							if skipEncode[i] ~= true then
								local encodeFn = encodeFns[i]
								local encoded = nil
								if encodeFn ~= false then
									encoded = encodeFn(value, state, strings, stringIds, enumTypes[i])
								else
									encoded = encodeValue(typeIds[i], enumTypes[i], value, state, strings, stringIds)
								end
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
	
			local compactValues = false
			if valuesOut ~= nil and valueWriteIndex > 0 then
				compactValues = valuesOut
			end
	
			local nameId = internBatchString(strings, stringIds, state.nameByIndex[instanceIndex] or instance.Name)
			if compactMask == false and compactValues == false then
				if attributes == false then
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
	
			if attributes == false then
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
	
	local function dynamicCompactTypeIdForValue(value: any): number?
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
		if type(attributes) ~= "table" or next(attributes) == nil then
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
			local typeId = dynamicCompactTypeIdForValue(value)
			if typeId ~= nil then
				local encoded
				if typeId == COMPACT_TYPE_IDS.EnumItem and typeof(value) == "EnumItem" then
					encoded = {
						Config.internBatchString(strings, stringIds, tostring(value.EnumType)),
						Config.internBatchString(strings, stringIds, value.Name),
					}
				else
					encoded = encodeSchemaValueV5(typeId, nil, value, state, strings, stringIds)
				end
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
	
	local function exportCompactV5InstanceWithHotSchema(
		state: ServiceState,
		instance: Instance,
		instanceIndex: number,
		className: string,
		hotSchema: { [string]: any },
		forceSafeReads: boolean,
		strings: { string },
		stringIds: { [string]: number }
	): { any }
		local exporter
		if forceSafeReads or hotSchema.forceSafeReads == true then
			exporter = hotSchema.exporterWithFallback
			if exporter == false or exporter == nil then
				exporter = Config.buildCompactV5Exporter(className, hotSchema, true)
				hotSchema.exporterWithFallback = exporter
			end
			return exporter(state, instance, instanceIndex, true, strings, stringIds)
		end
	
		exporter = hotSchema.exporter
		if exporter == false or exporter == nil then
			exporter = Config.buildCompactV5Exporter(className, hotSchema, false)
			hotSchema.exporter = exporter
		end
		return exporter(state, instance, instanceIndex, false, strings, stringIds)
	end
	
	local function exportCompactV5InstanceInternal(
		state: ServiceState,
		instance: Instance,
		instanceIndex: number,
		forceSafeReads: boolean,
		strings: { string },
		stringIds: { [string]: number }
	): { any }
		local className = state.classNameByIndex[instanceIndex] or instance.ClassName
		local hotSchema = Config.getHotPropertySchema(state, className)
		return exportCompactV5InstanceWithHotSchema(
			state,
			instance,
			instanceIndex,
			className,
			hotSchema,
			forceSafeReads,
			strings,
			stringIds
		)
	end
	
	local function exportCompactV5InstanceIndexed(
		state: ServiceState,
		inst: Instance,
		instanceIndex: number,
		strings: { string },
		stringIds: { [string]: number },
		knownClassName: string?,
		knownHotSchema: any?
	)
		local className = knownClassName or state.classNameByIndex[instanceIndex] or inst.ClassName
		local hotSchema = knownHotSchema or Config.getHotPropertySchema(state, className)
		if hotSchema.forceSafeReads == true then
			return exportCompactV5InstanceWithHotSchema(state, inst, instanceIndex, className, hotSchema, true, strings, stringIds)
		end
		if hotSchema.fastExportSafe == true then
			return exportCompactV5InstanceWithHotSchema(state, inst, instanceIndex, className, hotSchema, false, strings, stringIds)
		end
	
		local ok, entry =
			pcall(exportCompactV5InstanceWithHotSchema, state, inst, instanceIndex, className, hotSchema, false, strings, stringIds)
		if ok then
			hotSchema.fastExportSafe = true
			return entry
		end
	
		local learned = Config.learnClassPropertyFallbacks(state, inst, className, hotSchema.names)
		if learned then
			local exporterWithFallback = hotSchema.exporterWithFallback
			if exporterWithFallback == false or exporterWithFallback == nil then
				exporterWithFallback = Config.buildCompactV5Exporter(className, hotSchema, true)
				hotSchema.exporterWithFallback = exporterWithFallback
			end
			if hotSchema.fallbackExportSafe == true then
				return exporterWithFallback(state, inst, instanceIndex, false, strings, stringIds)
			end
			local retryOk, retryEntry = pcall(exporterWithFallback, state, inst, instanceIndex, false, strings, stringIds)
			if retryOk then
				hotSchema.fallbackExportSafe = true
				return retryEntry
			end
		end
		hotSchema.forceSafeReads = true
		return exportCompactV5InstanceWithHotSchema(state, inst, instanceIndex, className, hotSchema, true, strings, stringIds)
	end
	
	local function exportCompactV5Instance(
		state: ServiceState,
		inst: Instance,
		strings: { string },
		stringIds: { [string]: number }
	)
		local instanceIndex = IdentityModule.getCachedInstanceIndex(state, inst) or 1
		return exportCompactV5InstanceIndexed(state, inst, instanceIndex, strings, stringIds)
	end
	
	local function getCompactWarmWorkerCount(total: number): number
		local frameMs = perfState and tonumber(perfState.frameMs) or 16.67
		local maxWorkers = PRE_SERIALIZE_WARM_MAX_WORKERS
		if frameMs >= LAG_FRAME_MS then
			maxWorkers = 1
		elseif frameMs >= FAST_WARM_FRAME_MS then
			maxWorkers = math.min(maxWorkers, 2)
		elseif frameMs >= FAST_WARM_FRAME_MS * 0.75 then
			maxWorkers = math.min(maxWorkers, 3)
		end
		return math.min(maxWorkers, ParallelModule.getParallelChunkWorkerCount(total, PARALLEL_PRE_SERIALIZE_MIN_ITEMS))
	end
	
	local function isCompactDemandActive(state: ServiceState): boolean
		return (state.activeInstanceBatchRequests or state.compactDemandCount or 0) > 0
	end
	
	function Config.beginCompactDemand(state: ServiceState)
		local nextCount = (state.activeInstanceBatchRequests or state.compactDemandCount or 0) + 1
		state.activeInstanceBatchRequests = nextCount
		state.compactDemandCount = nextCount
	end
	
	function Config.endCompactDemand(state: ServiceState)
		local nextCount = math.max(0, (state.activeInstanceBatchRequests or state.compactDemandCount or 0) - 1)
		state.activeInstanceBatchRequests = nextCount
		state.compactDemandCount = nextCount
	end
	
	function Config.getCompactDemandWorkerCount(state: ServiceState, totalItems: number): number
		return 1
	end
	
	function Config.getDemandSerializerLimit(): number
		if PERFORMANCE_MODE == "throughput" then
			return MAX_ACTIVE_DEMAND_SERIALIZERS
		elseif PERFORMANCE_MODE == "balanced" then
			return DEFAULT_ACTIVE_DEMAND_SERIALIZERS
		end
		if perfState == nil then
			return DEFAULT_ACTIVE_DEMAND_SERIALIZERS
		end
		local maxFrameMs = tonumber(perfState.maxFrameMsSinceLastRead) or 0
		local lastFrameMs = tonumber(perfState.lastFrameMs) or 0
		local sampleCountSinceLastRead = tonumber(perfState.sampleCountSinceLastRead) or 0
		local stallCountOver50MsSinceLastRead = tonumber(perfState.stallCountOver50MsSinceLastRead) or 0
		if maxFrameMs >= THROTTLED_DEMAND_SERIALIZER_MAX_FRAME_MS or lastFrameMs >= THROTTLED_DEMAND_SERIALIZER_MAX_FRAME_MS then
			return 1
		end
		if sampleCountSinceLastRead > 0
			and stallCountOver50MsSinceLastRead <= 0
			and maxFrameMs > 0
			and maxFrameMs <= CLEAN_DEMAND_SERIALIZER_MAX_FRAME_MS
			and lastFrameMs <= CLEAN_DEMAND_SERIALIZER_MAX_FRAME_MS then
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
	
	local function ensureCompactSerializedTable(state: ServiceState): { [number]: any }
		local serialized = state.serializedCompactInstances
		if serialized == nil then
			serialized = table.create(#state.instances)
			state.serializedCompactInstances = serialized
		end
		return serialized
	end
	
	local function getOrCreateCompactSerializedEntry(state: ServiceState, index: number): any
		local serialized = ensureCompactSerializedTable(state)
		while true do
			local cached = serialized[index]
			if cached == nil then
				serialized[index] = COMPACT_SLOT_IN_PROGRESS
				local ok, entry = pcall(exportCompactInstanceEntry, state, state.instances[index])
				if ok then
					serialized[index] = entry
					return entry
				end
				serialized[index] = nil
				error(entry)
			elseif cached == COMPACT_SLOT_IN_PROGRESS then
				task.wait()
			else
				return cached
			end
		end
	end
	
	local function startCompactWarm(state: ServiceState)
		if state.compactWarmStatus == "scheduled" or state.compactWarmStatus == "warming" or state.compactWarmStatus == "ready" then
			return
		end
		if isCompactDemandActive(state) then
			return
		end
	
		local total = #state.instances
		if total <= 0 then
			state.serializedCompactInstances = {}
			state.compactWarmStatus = "ready"
			return
		end
	
		local serialized = ensureCompactSerializedTable(state)
		state.compactWarmStatus = "scheduled"
		task.defer(function()
			if isCompactDemandActive(state) then
				state.compactWarmStatus = "idle"
				return
			end
			state.compactWarmStatus = "warming"
			local ok, err = pcall(function()
				local workerCount = getCompactWarmWorkerCount(total)
				ParallelModule.runParallelChunks(total, workerCount, function(startIndex, endIndex)
					local yieldIfNeeded = ParallelModule.makeBurstYielder(32, SERIALIZATION_BURST_BUDGET_SECONDS)
					for i = startIndex, endIndex do
						if isCompactDemandActive(state) then
							return
						end
						if perfState and perfState.frameMs >= LAG_FRAME_MS then
							task.wait()
						end
						local cached = serialized[i]
						if cached == nil or cached == COMPACT_SLOT_IN_PROGRESS then
							getOrCreateCompactSerializedEntry(state, i)
						end
						yieldIfNeeded()
					end
				end)
			end)
			if ok then
				if isCompactDemandActive(state) then
					state.compactWarmStatus = "idle"
				else
					state.compactWarmStatus = "ready"
				end
			else
				state.compactWarmStatus = "idle"
				warn("[Renium] compact warm failed for " .. tostring(state.rootName) .. ": " .. tostring(err))
			end
		end)
	end
	
	prepareService = function(serviceName: string): { [string]: any }
		if not ALLOWED_SERVICES[serviceName] then
			error("Unsupported service: " .. tostring(serviceName))
		end
		local service = game:FindFirstChild(serviceName)
		if not service then
			error("Service not found: " .. serviceName)
		end
	
		local descendants = service:GetDescendants()
		local expectedCount = #descendants + 1
		local instances = table.create(expectedCount)
		instances[1] = service
		local instanceCount = 1
	
		local scriptObjects = {}
		local scriptCount = 0
		local classSeen = {}
		local classNames = {}
		local classIdByName = {}
		local nameByIndex = table.create(expectedCount)
		local classNameByIndex = table.create(expectedCount)
		local classValueByIndex = table.create(expectedCount)
		local parentIndexByIndex = table.create(expectedCount)
		local unresolvedParentIndices = {}
		local serviceClassName = service.ClassName
		local serviceIsLuaSourceContainer = Config.LUA_SOURCE_CLASS[serviceClassName] == true
		classSeen[serviceClassName] = true
		classNames[1] = serviceClassName
		classIdByName[serviceClassName] = 0
		local pathByInstance = { [service] = service.Name }
		local pathSegmentsByInstance = { [service] = { service.Name } }
		local debugIdByInstance: { [Instance]: string | boolean } = {}
		local instanceIdByInstance: { [Instance]: string | number | boolean } = {}
		local scriptKeyByInstance: { [Instance]: string } = {}
		instanceIdByInstance[service] = 1
		nameByIndex[1] = service.Name
		classNameByIndex[1] = serviceClassName
		classValueByIndex[1] = 0
		parentIndexByIndex[1] = false
	
		if serviceIsLuaSourceContainer then
			scriptCount += 1
			scriptObjects[scriptCount] = service
		end
	
		local yieldIfNeeded = makeExportBurstYielder()
		for _, inst in ipairs(descendants) do
			instanceCount += 1
			instances[instanceCount] = inst
			instanceIdByInstance[inst] = instanceCount
	
			local className = inst.ClassName
			local isLuaSourceContainer = Config.LUA_SOURCE_CLASS[className] == true
			local parent = inst.Parent
			if not classSeen[className] then
				classSeen[className] = true
				classNames[#classNames + 1] = className
				classIdByName[className] = #classNames - 1
			end
			nameByIndex[instanceCount] = inst.Name
			classNameByIndex[instanceCount] = className
			classValueByIndex[instanceCount] = classIdByName[className] or className
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
	
			if isLuaSourceContainer then
				scriptCount += 1
				scriptObjects[scriptCount] = inst
			end
			yieldIfNeeded()
		end
	
		stateByService[serviceName] = {
			instances = instances,
			classNames = classNames,
			classIdByName = classIdByName,
			generatedAtUnix = os.time(),
			rootName = service.Name,
			rootClassName = service.ClassName,
			rootPath = service.Name,
			pathByInstance = pathByInstance,
			pathSegmentsByInstance = pathSegmentsByInstance,
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
			scriptSourcesByIndex = {},
			scriptInstances = nil,
			scriptInstancesByIndex = nil,
			scriptKeyByInstance = scriptKeyByInstance,
			classDefaults = nil,
			classDefaultsEncoded = nil,
			transportDefaultProperties = nil,
			serializedInstances = nil,
			serializedCompactInstances = nil,
			compactWarmStatus = "idle",
			compactDemandCount = 0,
			activeInstanceBatchRequests = 0,
			scriptPathsEncoded = nil,
			batchCacheByKey = {},
			batchCacheKeys = {},
			sourceBatchCacheByKey = {},
			sourceBatchCacheKeys = {},
			servicePropertyCandidatesByClass = nil,
			servicePropertySchemaByClass = nil,
			hotPropertySchemaByClass = nil,
			safeReadByClass = {},
			requiresPcallByClassProperty = {},
			modifiedDefaultCheckCount = 0,
			modifiedDefaultElidedCount = 0,
			modifiedDefaultElidedByClass = {},
			modifiedDefaultValidationSamplesByKey = {},
			modifiedDefaultAdaptiveStatsByKey = {},
			modifiedDefaultAdaptiveDecisionByKey = {},
			modifiedDefaultRuntimeDenylist = {},
			exportMetrics = Config.newExportMetrics(),
			exportMetricsSinceLastRead = Config.newExportMetrics(),
		}
	
		local state = stateByService[serviceName]
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
		if PRE_SERIALIZE_ON_PREPARE then
			if instanceCount <= PRE_SERIALIZE_MAX_INSTANCES then
				local serialized = table.create(instanceCount)
				local workerCount = ParallelModule.getParallelChunkWorkerCount(instanceCount, PARALLEL_PRE_SERIALIZE_MIN_ITEMS)
				ParallelModule.runParallelChunks(instanceCount, workerCount, function(startIndex, endIndex)
					for i = startIndex, endIndex do
						local inst = instances[i]
						local path = IdentityModule.getCachedInstancePath(state, inst)
						local parentPath = IdentityModule.getCachedParentPath(state, inst)
						local debugId = IdentityModule.getCachedDebugId(state, inst)
						local parentDebugId = IdentityModule.getCachedParentDebugId(state, inst)
						local instanceId = IdentityModule.getCachedInstanceId(state, inst)
						local parentInstanceId = IdentityModule.getCachedParentInstanceId(state, inst)
						serialized[i] = exportInstanceWithoutSharedFallback(
							state,
							inst,
							path,
							parentPath,
							debugId,
							parentDebugId,
							instanceId,
							parentInstanceId,
							true
						)
					end
				end)
				state.serializedInstances = serialized
			elseif PRE_SERIALIZE_LARGE_SERVICE_WARM and instanceCount >= PRE_SERIALIZE_WARM_MIN_INSTANCES then
				startCompactWarm(state)
			end
		end
	
		local preSerializedMode = "off"
		if state.serializedInstances ~= nil then
			preSerializedMode = "full"
		elseif state.compactWarmStatus == "scheduled" or state.compactWarmStatus == "warming" then
			preSerializedMode = "warming"
		elseif state.compactWarmStatus == "ready" and state.serializedCompactInstances ~= nil then
			preSerializedMode = "compact"
		end
	
		return {
			service = serviceName,
			bridgeVersion = BRIDGE_VERSION,
			protocolVersion = BRIDGE_PROTOCOL_VERSION,
			codecVersion = CODEC_VERSION,
			bridgeBuildUnix = BRIDGE_BUILD_UNIX,
			generatedAtUnix = state.generatedAtUnix,
			rootName = state.rootName,
			rootClassName = state.rootClassName,
			rootPath = state.rootPath,
			instanceCount = instanceCount,
			scriptCount = scriptCount,
			classNames = state.classNames,
			performanceMode = PERFORMANCE_MODE,
			modifiedDefaultBypass = MODIFIED_DEFAULT_BYPASS_ENABLED,
			preSerialized = preSerializedMode ~= "off",
			preSerializedMode = preSerializedMode,
			propertyCandidatesByClass = getServicePropertyCandidates(state),
			propertySchemaByClass = getServicePropertySchema(state),
			enumValueNamesByType = getServiceEnumValueNamesByType(state),
		}
	end
	
	function Config.getBridgeInfo(): { [string]: any }
		local playerName, playerUserId = Config.getPlayerIdentity()
		return {
			playerName = playerName,
			playerUserId = playerUserId,
			placeId = game.PlaceId,
			gameId = game.GameId,
			placeName = workspace:GetAttribute("__ReniumPlace") or game.Name,
			bridgeVersion = BRIDGE_VERSION,
			bridgeBuildUnix = BRIDGE_BUILD_UNIX,
			protocolVersion = BRIDGE_PROTOCOL_VERSION,
			codecVersion = CODEC_VERSION,
			supportedInstanceProtocols = { BRIDGE_PROTOCOL_VERSION, "compact-v5-shape" },
			chunkFrameProtocolVersion = CHUNK_FRAME_PROTOCOL_VERSION,
			compactValueProtocolVersion = COMPACT_VALUE_PROTOCOL_VERSION,
			largeServiceWarmMode = PRE_SERIALIZE_LARGE_SERVICE_WARM and "coordinated" or "disabled",
			serializerWorkerMode = SERIALIZER_WORKER_MODE,
			performanceMode = PERFORMANCE_MODE,
			bridgeRole = Config.bridgeRole,
			exportAllProperties = EXPORT_ALL_PROPERTIES,
			modifiedDefaultBypass = MODIFIED_DEFAULT_BYPASS_ENABLED,
			preSerializeOnPrepare = PRE_SERIALIZE_ON_PREPARE,
			preSerializeLargeServiceWarm = PRE_SERIALIZE_LARGE_SERVICE_WARM,
			runtimeSettings = Config.getBridgeSettings and Config.getBridgeSettings() or {},
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
	
	ProfilingModule.install(Config, {
		httpService = HttpService,
		identityModule = IdentityModule,
		compactTypeIds = COMPACT_TYPE_IDS,
		bridgeVersion = BRIDGE_VERSION,
		bridgeBuildUnix = BRIDGE_BUILD_UNIX,
		protocolVersion = BRIDGE_PROTOCOL_VERSION,
		codecVersion = CODEC_VERSION,
		getState = getState,
		tryRead = tryRead,
		serializeValue = serializeValue,
		encodeSchemaValueV5 = encodeSchemaValueV5,
		serializeAttributesCompactV5 = serializeAttributesCompactV5,
		exportCompactV5InstanceInternal = exportCompactV5InstanceInternal,
	})
	local function boundedPositiveInteger(value: any, defaultValue: number, maximum: number): number
		local numeric = tonumber(value)
		if numeric == nil or numeric ~= numeric then
			return defaultValue
		end
		return math.clamp(math.floor(numeric), 1, maximum)
	end

	local function getInstanceBatch(serviceName: string, startIndex: number?, maxCount: number?): (string, number)
		local state = getState(serviceName)
		local key = ChunkingModule.getInstanceBatchCacheKey(startIndex, maxCount)
		local cachedPayload = state.batchCacheByKey[key]
		if cachedPayload then
			return cachedPayload, 0
		end
	
		local instances = state.instances
		local total = #instances
		local startPos = boundedPositiveInteger(startIndex, 1, math.max(total + 1, 1))
		local take = boundedPositiveInteger(maxCount, 300, MAX_INSTANCE_BATCH_ITEMS)
		local encoded: string
		local encodeMs = 0
	
		if startPos > total then
			encoded, encodeMs = ChunkingModule.jsonEncodeTimed({ start = startPos, nextStart = startPos, total = total, items = {} })
		else
			local finish = math.min(total, startPos + take - 1)
			local items = table.create(finish - startPos + 1)
			local count = finish - startPos + 1
			if state.serializedInstances then
				for i = startPos, finish do
					items[#items + 1] = state.serializedInstances[i]
				end
			else
				local workerCount = ParallelModule.getParallelChunkWorkerCount(count, PARALLEL_INSTANCE_BATCH_MIN_ITEMS)
				ParallelModule.runParallelChunks(count, workerCount, function(startOffset, endOffset)
					for offset = startOffset, endOffset do
						local i = startPos + offset - 1
						local inst = instances[i]
						local path = IdentityModule.getCachedInstancePath(state, inst)
						local parentPath = IdentityModule.getCachedParentPath(state, inst)
						local debugId = IdentityModule.getCachedDebugId(state, inst)
						local parentDebugId = IdentityModule.getCachedParentDebugId(state, inst)
						local instanceId = IdentityModule.getCachedInstanceId(state, inst)
						local parentInstanceId = IdentityModule.getCachedParentInstanceId(state, inst)
						items[offset] = exportInstanceWithoutSharedFallback(
							state,
							inst,
							path,
							parentPath,
							debugId,
							parentDebugId,
							instanceId,
							parentInstanceId,
							true
						)
					end
				end)
			end
			encoded, encodeMs = ChunkingModule.jsonEncodeTimed({
				start = startPos,
				nextStart = finish + 1,
				total = total,
				items = items,
			})
		end
	
		state.batchCacheByKey[key] = encoded
		state.batchCacheKeys[#state.batchCacheKeys + 1] = key
		if #state.batchCacheKeys > 256 then
			local oldestKey = table.remove(state.batchCacheKeys, 1)
			if oldestKey and oldestKey ~= key then
				state.batchCacheByKey[oldestKey] = nil
			end
		end
		return encoded, encodeMs
	end
	
	local function getCompactInstanceBatchVariantCacheKey(
		startIndex: number?,
		maxCount: number?,
		shapeBatchesEnabled: boolean?,
		stableIdsEnabled: boolean?
	): string
		local key = ChunkingModule.getCompactInstanceBatchCacheKey(startIndex, maxCount)
		if shapeBatchesEnabled == true then
			key ..= ":shape-v1"
		else
			key ..= ":plain-v1"
		end
		if stableIdsEnabled == true then
			key ..= ":stable-v1"
		end
		return key
	end
	
	local function getInstanceBatchCompact(
		serviceName: string,
		startIndex: number?,
		maxCount: number?,
		shapeBatchesEnabled: boolean?,
		stableIdsEnabled: boolean?
	): (string, number)
		local state = getState(serviceName)
		local useShapeBatches = shapeBatchesEnabled == true
		local includeStableIds = stableIdsEnabled == true
		local key = getCompactInstanceBatchVariantCacheKey(startIndex, maxCount, useShapeBatches, includeStableIds)
		local cachedPayload = state.batchCacheByKey[key]
		if cachedPayload then
			return cachedPayload, 0
		end
	
		local instances = state.instances
		local total = #instances
		local startPos = boundedPositiveInteger(startIndex, 1, math.max(total + 1, 1))
		local take = boundedPositiveInteger(maxCount, 300, MAX_INSTANCE_BATCH_ITEMS)
		local encoded: string
		local encodeMs = 0
	
		Config.beginCompactDemand(state)
		local acquiredSerializerSlot = false
		local ok, resultOrErr, maybeEncodeMs = pcall(function(): (string, number)
			Config.acquireDemandSerializerSlot()
			acquiredSerializerSlot = true
			if startPos > total then
				return ChunkingModule.jsonEncodeTimed({
					format = BRIDGE_PROTOCOL_VERSION,
					codecVersion = CODEC_VERSION,
					start = startPos,
					nextStart = startPos,
					total = total,
					strings = {},
					debugIds = if includeStableIds then {} else nil,
					items = {},
				})
			end
	
			local finish = math.min(total, startPos + take - 1)
			local count = finish - startPos + 1
			local items = table.create(count)
			local debugIds = if includeStableIds then table.create(count) else nil
			local strings = table.create(math.min(count * 2, 65536))
			local stringIds = {}
			local workerCount = Config.getCompactDemandWorkerCount(state, count)
			if MODIFIED_DEFAULT_BYPASS_ENABLED then
				local workerMetrics = {}
				ParallelModule.runParallelChunks(count, workerCount, function(startOffset, endOffset)
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
						local inst = instances[i]
						local className = state.classNameByIndex[i] or inst.ClassName
						local hotSchema = lastHotSchema
						if debugIds ~= nil then
							local debugId = IdentityModule.getCachedDebugId(state, inst)
							debugIds[offset] = if debugId ~= nil then Config.internBatchString(strings, stringIds, debugId) else false
						end
						if className ~= lastClassName then
							hotSchema = Config.getHotPropertySchema(state, className)
							lastClassName = className
							lastHotSchema = hotSchema
						end
						local item,
							itemModifiedChecks,
							itemModifiedElided,
							itemModifiedValidationReads,
							itemModifiedRuntimeDenylistCount,
							itemPropertiesRead,
							itemPropertiesEncoded,
							itemPropertiesDefaultSkipped = exportCompactV5InstanceIndexed(state, inst, i, strings, stringIds, className, hotSchema)
						items[offset] = item
						modifiedDefaultChecks += itemModifiedChecks or 0
						modifiedDefaultElided += itemModifiedElided or 0
						modifiedDefaultValidationReads += itemModifiedValidationReads or 0
						modifiedDefaultRuntimeDenylistCount += itemModifiedRuntimeDenylistCount or 0
						propertiesRead += itemPropertiesRead or 0
						propertiesEncoded += itemPropertiesEncoded or 0
						propertiesDefaultSkipped += itemPropertiesDefaultSkipped or 0
						if yieldIfNeeded ~= nil then
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
					for key, value in pairs(metrics) do
						mergedMetrics[key] = (mergedMetrics[key] or 0) + value
					end
				end
				Config.mergeExportMetrics(state, mergedMetrics)
			else
				ParallelModule.runParallelChunks(count, workerCount, function(startOffset, endOffset)
					local lastClassName = nil
					local lastHotSchema = nil
					local yieldIfNeeded = nil
					if Config.shouldYieldDuringDemandSerialization() then
						local checkInterval, budgetSeconds = Config.demandSerializationYieldConfig()
						yieldIfNeeded = ParallelModule.makeBurstYielder(checkInterval, budgetSeconds)
					end
					for offset = startOffset, endOffset do
						local i = startPos + offset - 1
						local inst = instances[i]
						local className = state.classNameByIndex[i] or inst.ClassName
						local hotSchema = lastHotSchema
						if debugIds ~= nil then
							local debugId = IdentityModule.getCachedDebugId(state, inst)
							debugIds[offset] = if debugId ~= nil then Config.internBatchString(strings, stringIds, debugId) else false
						end
						if className ~= lastClassName then
							hotSchema = Config.getHotPropertySchema(state, className)
							lastClassName = className
							lastHotSchema = hotSchema
						end
						items[offset] = exportCompactV5InstanceIndexed(state, inst, i, strings, stringIds, className, hotSchema)
						if yieldIfNeeded ~= nil then
							yieldIfNeeded()
						end
					end
				end)
			end
			if useShapeBatches then
				local shapedItems, shapes = Config.tryBuildCompactShapeBatch(items, count)
				if shapedItems ~= nil and shapes ~= nil then
					return ChunkingModule.jsonEncodeTimed({
						format = "compact-v5-shape",
						codecVersion = CODEC_VERSION,
						start = startPos,
						nextStart = finish + 1,
						total = total,
						defaultElided = true,
						defaultElisionVersion = 1,
						strings = strings,
						shapes = shapes,
						debugIds = debugIds,
						items = shapedItems,
					})
				end
			end
	
			return ChunkingModule.jsonEncodeTimed({
				format = BRIDGE_PROTOCOL_VERSION,
				codecVersion = CODEC_VERSION,
				start = startPos,
				nextStart = finish + 1,
				total = total,
				defaultElided = true,
				defaultElisionVersion = 1,
				strings = strings,
				debugIds = debugIds,
				items = items,
			})
		end)
		if acquiredSerializerSlot then
			Config.releaseDemandSerializerSlot()
		end
		Config.endCompactDemand(state)
		if not ok then
			error(resultOrErr)
		end
		encoded = resultOrErr
		encodeMs = maybeEncodeMs
	
		state.batchCacheByKey[key] = encoded
		state.batchCacheKeys[#state.batchCacheKeys + 1] = key
		if #state.batchCacheKeys > 256 then
			local oldestKey = table.remove(state.batchCacheKeys, 1)
			if oldestKey and oldestKey ~= key then
				state.batchCacheByKey[oldestKey] = nil
			end
		end
		return encoded, encodeMs
	end
	
	local function getClassDefaults(serviceName: string): (string, number)
		local state = getState(serviceName)
		if state.classDefaultsEncoded then
			return state.classDefaultsEncoded, 0
		end
		if not state.classDefaults then
			local classDefaults = {}
			for _, className in ipairs(state.classNames) do
				local defaults = getDefaultSerializedProperties(className)
				if defaults ~= nil and next(defaults) ~= nil then
					classDefaults[className] = defaults
				end
			end
			state.classDefaults = classDefaults
		end
		local encoded, encodeMs = ChunkingModule.jsonEncodeTimed(state.classDefaults)
		state.classDefaultsEncoded = encoded
		return state.classDefaultsEncoded, encodeMs
	end
	
	local function getScriptPaths(serviceName: string): (string, number)
		local state = getState(serviceName)
		ensureScriptIndex(state)
		if not state.scriptPathsEncoded then
			local encoded = nil
			local encodeMs = 0
			encoded, encodeMs = ChunkingModule.jsonEncodeTimed(state.scriptPaths)
			state.scriptPathsEncoded = encoded
			return state.scriptPathsEncoded, encodeMs
		end
		return state.scriptPathsEncoded, 0
	end
	
	local function getSourceForKey(state: ServiceState, sourceKey: string): string
		local src = state.scriptSources[sourceKey]
		if src ~= nil then
			return src
		end
	
		local scriptInstance = state.scriptInstances and state.scriptInstances[sourceKey] or nil
		if scriptInstance == nil then
			src = ""
		else
			local ok, loaded = pcall(function()
				return scriptInstance.Source
			end)
			src = ok and loaded or ""
		end
		state.scriptSources[sourceKey] = src
		return src
	end
	
	local function getSourceForIndex(state: ServiceState, sourceIndex: number): string
		local src = state.scriptSourcesByIndex[sourceIndex]
		if src ~= nil then
			return src
		end
	
		local scriptInstance = state.scriptInstancesByIndex and state.scriptInstancesByIndex[sourceIndex] or nil
		if scriptInstance == nil then
			src = ""
		else
			local ok, loaded = pcall(function()
				return scriptInstance.Source
			end)
			src = ok and loaded or ""
		end
		state.scriptSourcesByIndex[sourceIndex] = src
		return src
	end
	
	local function getSourceChunk(serviceName: string, instancePath: string, startIndex: number?, maxLen: number?): { [string]: any }
		local state = getState(serviceName)
		ensureScriptIndex(state)
		if #instancePath > MAX_SOURCE_KEY_BYTES then
			error("Source key exceeds safe size limit")
		end
		return ChunkingModule.chunkEncodedString(getSourceForKey(state, instancePath), startIndex, maxLen, 0)
	end

	local function getSourceBatchEncoded(serviceName: string, instancePaths: { any }): (string, number)
		local state = getState(serviceName)
		ensureScriptIndex(state)
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
		local cacheKey = ChunkingModule.getSourceBatchCacheKey(instancePaths)
		local cachedPayload = state.sourceBatchCacheByKey[cacheKey]
		if cachedPayload then
			return cachedPayload, 0
		end
	
		local out = {}
		local sourcesByIndex = table.create(#normalizedPaths)
		local workerCount = ParallelModule.getParallelChunkWorkerCount(#normalizedPaths, PARALLEL_SOURCE_BATCH_MIN_ITEMS)
		ParallelModule.runParallelChunks(#normalizedPaths, workerCount, function(startIndex, endIndex)
			for i = startIndex, endIndex do
				local sourceKey = normalizedPaths[i]
				sourcesByIndex[i] = getSourceForKey(state, sourceKey)
			end
		end)
		for i, sourceKey in ipairs(normalizedPaths) do
			out[sourceKey] = sourcesByIndex[i] or ""
		end
	
		local encoded, encodeMs = ChunkingModule.jsonEncodeTimed(out)
		state.sourceBatchCacheByKey[cacheKey] = encoded
		state.sourceBatchCacheKeys[#state.sourceBatchCacheKeys + 1] = cacheKey
		if #state.sourceBatchCacheKeys > 64 then
			local oldestKey = table.remove(state.sourceBatchCacheKeys, 1)
			if oldestKey and oldestKey ~= cacheKey then
				state.sourceBatchCacheByKey[oldestKey] = nil
			end
		end
		return encoded, encodeMs
	end
	
	local function getSourceRangeBatchCompact(serviceName: string, startIndex: number?, maxCount: number?): (string, number)
		local state = getState(serviceName)
		ensureScriptIndex(state)
		local total = state.scriptIndices and #state.scriptIndices or 0
		local startPos = boundedPositiveInteger(startIndex, 1, math.max(total + 1, 1))
		local take = boundedPositiveInteger(maxCount, 64, MAX_SOURCE_BATCH_PATHS)
		local cacheKey = ChunkingModule.getSourceRangeBatchCacheKey(startPos, take)
		local cachedPayload = state.sourceBatchCacheByKey[cacheKey]
		if cachedPayload then
			return cachedPayload, 0
		end
	
		local encoded: string
		local encodeMs = 0
		if startPos > total then
			encoded, encodeMs = ChunkingModule.jsonEncodeTimed({
				format = "source-pairs-v2",
				start = startPos,
				nextStart = startPos,
				total = total,
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
					sourcesByIndex[offset] = getSourceForIndex(state, sourceIndex)
				end
			end)
	
			local items = table.create(count * 2)
			for offset = 1, count do
				items[#items + 1] = indicesByIndex[offset] or 0
				items[#items + 1] = sourcesByIndex[offset] or ""
			end
			encoded, encodeMs = ChunkingModule.jsonEncodeTimed({
				format = "source-pairs-v2",
				start = startPos,
				nextStart = finish + 1,
				total = total,
				items = items,
			})
		end
	
		state.sourceBatchCacheByKey[cacheKey] = encoded
		state.sourceBatchCacheKeys[#state.sourceBatchCacheKeys + 1] = cacheKey
		if #state.sourceBatchCacheKeys > 64 then
			local oldestKey = table.remove(state.sourceBatchCacheKeys, 1)
			if oldestKey and oldestKey ~= cacheKey then
				state.sourceBatchCacheByKey[oldestKey] = nil
			end
		end
		return encoded, encodeMs
	end
	
	local function getSourceBatchChunk(
		serviceName: string,
		instancePaths: { any },
		startIndex: number?,
		maxLen: number?
	): { [string]: any }
		local state = getState(serviceName)
		local encoded, encodeMs = getSourceBatchEncoded(serviceName, instancePaths)
		local cacheKey = ChunkingModule.getSourceBatchCacheKey(instancePaths)
		local chunk = ChunkingModule.chunkEncodedString(encoded, startIndex, maxLen, encodeMs)
		if chunk.nextStart > chunk.total then
			ChunkingModule.removeCachedPayload(state.sourceBatchCacheByKey, state.sourceBatchCacheKeys, cacheKey)
		end
		return chunk
	end
	
	local function getInstanceBatchChunk(serviceName: string, startIndex: number?, maxCount: number?, chunkStart: number?, maxLen: number?): { [string]: any }
		local state = getState(serviceName)
		local cacheKey = ChunkingModule.getInstanceBatchCacheKey(startIndex, maxCount)
		local encoded, encodeMs = getInstanceBatch(serviceName, startIndex, maxCount)
		local chunk = ChunkingModule.chunkEncodedString(encoded, chunkStart, maxLen, encodeMs)
		if chunk.nextStart > chunk.total then
			ChunkingModule.removeCachedPayload(state.batchCacheByKey, state.batchCacheKeys, cacheKey)
		end
		return chunk
	end
	
	local function getInstanceBatchCompactChunk(
		serviceName: string,
		startIndex: number?,
		maxCount: number?,
		chunkStart: number?,
		maxLen: number?,
		shapeBatchesEnabled: boolean?,
		stableIdsEnabled: boolean?
	): { [string]: any }
		local state = getState(serviceName)
		local cacheKey = getCompactInstanceBatchVariantCacheKey(startIndex, maxCount, shapeBatchesEnabled == true, stableIdsEnabled == true)
		local encoded, encodeMs = getInstanceBatchCompact(serviceName, startIndex, maxCount, shapeBatchesEnabled == true, stableIdsEnabled == true)
		local chunk = ChunkingModule.chunkEncodedString(encoded, chunkStart, maxLen, encodeMs)
		if chunk.nextStart > chunk.total then
			ChunkingModule.removeCachedPayload(state.batchCacheByKey, state.batchCacheKeys, cacheKey)
		end
		return chunk
	end
	
	local function getClassDefaultsChunk(serviceName: string, startIndex: number?, maxLen: number?): { [string]: any }
		local state = getState(serviceName)
		local encoded, encodeMs = getClassDefaults(serviceName)
		local chunk = ChunkingModule.chunkEncodedString(encoded, startIndex, maxLen, encodeMs)
		if chunk.nextStart > chunk.total then
			state.classDefaultsEncoded = nil
		end
		return chunk
	end
	
	local function getScriptPathsChunk(serviceName: string, startIndex: number?, maxLen: number?): { [string]: any }
		local state = getState(serviceName)
		local encoded, encodeMs = getScriptPaths(serviceName)
		local chunk = ChunkingModule.chunkEncodedString(encoded, startIndex, maxLen, encodeMs)
		if chunk.nextStart > chunk.total then
			state.scriptPathsEncoded = nil
		end
		return chunk
	end
	
	local function getSourceRangeBatchCompactChunk(
		serviceName: string,
		startIndex: number?,
		maxCount: number?,
		chunkStart: number?,
		maxLen: number?
	): { [string]: any }
		local state = getState(serviceName)
		local cacheKey = ChunkingModule.getSourceRangeBatchCacheKey(startIndex, maxCount)
		local encoded, encodeMs = getSourceRangeBatchCompact(serviceName, startIndex, maxCount)
		local chunk = ChunkingModule.chunkEncodedString(encoded, chunkStart, maxLen, encodeMs)
		if chunk.nextStart > chunk.total then
			ChunkingModule.removeCachedPayload(state.sourceBatchCacheByKey, state.sourceBatchCacheKeys, cacheKey)
		end
		return chunk
	end
	
	local perfState
	
	function Config.handleMethod(method: string, params: { [string]: any }?): any
		local p = params or {}
		if method == "ping" then
			return { ok = true, timestamp = os.time() }
		elseif method == "getBridgeInfo" then
			return Config.getBridgeInfo()
		elseif method == "getPerformanceStats" then
			local exportMetrics = Config.collectAndResetExportMetrics()
			local stats = {
				fps = perfState.fps,
				frameMs = perfState.frameMs,
				lastFrameMs = perfState.lastFrameMs,
				maxFrameMs = perfState.maxFrameMsSinceLastRead,
				lastHeartbeat = perfState.lastHeartbeat,
				sampleCount = perfState.sampleCount,
				sampleCountSinceLastRead = perfState.sampleCountSinceLastRead,
				stallCountOver33Ms = perfState.stallCountOver33MsSinceLastRead,
				stallCountOver50Ms = perfState.stallCountOver50MsSinceLastRead,
				stallCountOver100Ms = perfState.stallCountOver100MsSinceLastRead,
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
			perfState.maxFrameMsSinceLastRead = 0
			perfState.sampleCountSinceLastRead = 0
			perfState.stallCountOver33MsSinceLastRead = 0
			perfState.stallCountOver50MsSinceLastRead = 0
			perfState.stallCountOver100MsSinceLastRead = 0
			return stats
		elseif method == "configurePropertyCandidates" then
			return configurePropertyCandidates(p.classes)
		elseif method == "setExportOptions" then
			return configureExportOptions(p)
		elseif method == "requestEditorPushReview" then
			return ui.requestEditorPushReview(p, Config.getBridgeSettings and Config.getBridgeSettings() or {}, {
				decodeValue = EditorSyncModule.decodeValue,
				valuesEqual = EditorSyncModule.valuesEqual,
			})
		elseif method == "getEditorPushReviewDecision" then
			return ui.getEditorPushReviewDecision(p)
		elseif method == "applyEditorChanges" then
			Config.studioChanges.beginSuppress()
			local ok, result = pcall(function()
				return editorSync.applyChanges(p)
			end)
			task.defer(function()
				Config.studioChanges.endSuppress(0)
			end)
			if not ok then
				error(result, 0)
			end
			return result
		elseif method == "getStudioChangeState" then
			local runtimeSettings = Config.getBridgeSettings and Config.getBridgeSettings() or {}
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
					runtimeSettings = runtimeSettings,
				}
			end
			local changeState = Config.studioChanges.getState(p)
			changeState.twoWaySyncEnabled = true
			changeState.runtimeSettings = runtimeSettings
			return changeState
		elseif method == "setConflictResolution" then
			Config.studioChanges.setConflictResolution(tostring(p.value or ""))
			SettingsModule.saveConflictResolution(
				plugin,
				SETTINGS_PREFIX,
				Config.studioChanges.getConflictResolution()
			)
			return { ok = true, conflictResolution = Config.studioChanges.getConflictResolution() }
		elseif method == "getConsoleOutput" then
			return RuntimeApi.getConsoleOutput(p)
		elseif method == "getGuiBounds" then
			return RuntimeApi.getGuiBounds(p)
		elseif method == "getGuiInventory" then
			return RuntimeApi.getGuiInventory(p)
		elseif method == "getWorldPoint" then
			return RuntimeApi.getWorldPoint(p)
		elseif method == "getMouseLocation" then
			return RuntimeApi.getMouseLocation(p)
		elseif method == "executeLuau" then
			return RuntimeApi.executeLuau(p)
		elseif method == "startStopPlay" then
			return RuntimeApi.startStopPlay(p)
		elseif method == "prepareForNextRun" then
			return "ok"
		elseif method == "prepare" then
			return prepareService(tostring(p.service))
		elseif method == "profilePluginOps" then
			return Config.profilePluginOps(tostring(p.service), tonumber(p.sampleCount), tonumber(p.iterations), p.flags)
		elseif method == "getInstanceBatchChunk" then
			return getInstanceBatchChunk(tostring(p.service), p.startIndex, p.maxCount, p.chunkStart, p.maxLen)
		elseif method == "getInstanceBatchCompactChunk" then
			local shapeBatchesEnabled = p.supportsShapeBatches == true
				or p.shapeCompact == true
				or p.shapeCompactV5 == true
			if not shapeBatchesEnabled and type(p.supportedFormats) == "table" then
				for _, formatName in pairs(p.supportedFormats) do
					if formatName == "compact-v5-shape" or formatName == "compact-v6-shape" then
						shapeBatchesEnabled = true
						break
					end
				end
			end
			return getInstanceBatchCompactChunk(
				tostring(p.service),
				p.startIndex,
				p.maxCount,
				p.chunkStart,
				p.maxLen,
				shapeBatchesEnabled,
				p.supportsStableInstanceIds == true or p.stableInstanceIds == true
			)
		elseif method == "getClassDefaultsChunk" then
			return getClassDefaultsChunk(tostring(p.service), p.startIndex, p.maxLen)
		elseif method == "getScriptPathsChunk" then
			return getScriptPathsChunk(tostring(p.service), p.startIndex, p.maxLen)
		elseif method == "getSourceBatchChunk" then
			return getSourceBatchChunk(tostring(p.service), p.instancePaths or {}, p.startIndex, p.maxLen)
		elseif method == "getSourceRangeBatchCompactChunk" then
			return getSourceRangeBatchCompactChunk(tostring(p.service), p.startIndex, p.maxCount, p.chunkStart, p.maxLen)
		elseif method == "getSourceChunk" then
			return getSourceChunk(tostring(p.service), tostring(p.instancePath), p.startIndex, p.maxLen)
		elseif method == "release" then
			local serviceName = tostring(p.service)
			local state = stateByService[serviceName]
			if state ~= nil then
				if stateByService[serviceName] == state then
					stateByService[serviceName] = nil
				end
				local metrics = state.exportMetrics
				local hasMetrics = state.modifiedDefaultElidedCount > 0
					or (metrics.modifiedDefaultChecks or 0) > 0
					or (metrics.modifiedDefaultElided or 0) > 0
					or (metrics.modifiedDefaultValidationReads or 0) > 0
					or (metrics.modifiedDefaultRuntimeDenylistCount or 0) > 0
					or (metrics.propertiesRead or 0) > 0
					or (metrics.propertiesEncoded or 0) > 0
					or (metrics.propertiesDefaultSkipped or 0) > 0
					or (metrics.safeReadClassFallbackCount or 0) > 0
					or (metrics.safeReadPropertyFallbackCount or 0) > 0
				if false and hasMetrics then
					print(
						("[Renium] %s export metrics: modified_checks=%d modified_elided=%d modified_validation_reads=%d modified_denylist=%d properties_read=%d properties_encoded=%d properties_default_skipped=%d pcall_class_fallbacks=%d pcall_property_fallbacks=%d"):format(
							serviceName,
							metrics.modifiedDefaultChecks or 0,
							metrics.modifiedDefaultElided or 0,
							metrics.modifiedDefaultValidationReads or 0,
							metrics.modifiedDefaultRuntimeDenylistCount or 0,
							metrics.propertiesRead or 0,
							metrics.propertiesEncoded or 0,
							metrics.propertiesDefaultSkipped or 0,
							metrics.safeReadClassFallbackCount or 0,
							metrics.safeReadPropertyFallbackCount or 0
						)
					)
					if state.modifiedDefaultElidedCount > 0 then
						local classPairs = {}
						for className, count in pairs(state.modifiedDefaultElidedByClass) do
							classPairs[#classPairs + 1] = { className, count }
						end
						table.sort(classPairs, function(a, b)
							if a[2] == b[2] then
								return tostring(a[1]) < tostring(b[1])
							end
							return a[2] > b[2]
						end)
						local topParts = {}
						for i = 1, math.min(5, #classPairs) do
							local pair = classPairs[i]
							topParts[i] = tostring(pair[1]) .. ":" .. tostring(pair[2])
						end
						print(
							("[Renium] %s modified-default bypass: checks=%d omitted=%d top=%s"):format(
								serviceName,
								state.modifiedDefaultCheckCount,
								state.modifiedDefaultElidedCount,
								#topParts > 0 and table.concat(topParts, ",") or "none"
							)
						)
					end
				end
			end
			return "ok"
		else
			error("Unknown method: " .. tostring(method))
		end
	end
	
	perfState = {
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
	
	RunService.Heartbeat:Connect(function(dt: number)
		if dt <= 0 then
			return
		end
		local frameMs = dt * 1000
		local instantFps = 1 / dt
		local alpha = 0.08
		perfState.fps = perfState.fps + (instantFps - perfState.fps) * alpha
		if perfState.fps <= 0 then
			perfState.fps = instantFps
		end
		perfState.frameMs = 1000 / perfState.fps
		perfState.lastFrameMs = frameMs
		perfState.maxFrameMsSinceLastRead = math.max(perfState.maxFrameMsSinceLastRead or 0, frameMs)
		perfState.lastHeartbeat = os.clock()
		perfState.sampleCount += 1
		perfState.sampleCountSinceLastRead += 1
		if frameMs > 33 then
			perfState.stallCountOver33MsSinceLastRead += 1
		end
		if frameMs > 50 then
			perfState.stallCountOver50MsSinceLastRead += 1
		end
		if frameMs > 100 then
			perfState.stallCountOver100MsSinceLastRead += 1
		end
	end)
	
	local BRIDGE_ALLOWED_METHODS = {
		ping = true,
		getBridgeInfo = true,
		getPerformanceStats = true,
		configurePropertyCandidates = true,
		setExportOptions = true,
		applyEditorChanges = true,
		requestEditorPushReview = true,
		getEditorPushReviewDecision = true,
		getStudioChangeState = true,
		setConflictResolution = true,
		getConsoleOutput = true,
		getGuiBounds = true,
		getGuiInventory = true,
		getWorldPoint = true,
		getMouseLocation = true,
		executeLuau = true,
		startStopPlay = true,
		prepareForNextRun = true,
		prepare = true,
		profilePluginOps = true,
		getInstanceBatchChunk = true,
		getInstanceBatchCompactChunk = true,
		getClassDefaultsChunk = true,
		getScriptPathsChunk = true,
		getSourceBatchChunk = true,
		getSourceRangeBatchCompactChunk = true,
		getSourceChunk = true,
		release = true,
	}
	local BRIDGE_EXCLUSIVE_METHODS = {
		configurePropertyCandidates = true,
		setExportOptions = true,
		applyEditorChanges = true,
		setConflictResolution = true,
		executeLuau = true,
		startStopPlay = true,
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
		runService = RunService,
		settingsPrefix = SETTINGS_PREFIX,
		defaultHost = DEFAULT_HOST,
		defaultPorts = DEFAULT_PORTS,
		reconnectSeconds = RECONNECT_SECONDS,
		fastReconnectSeconds = FAST_RECONNECT_SECONDS,
		fastReconnectWindowSeconds = FAST_RECONNECT_WINDOW_SECONDS,
		connectOpenTimeoutSeconds = CONNECT_OPEN_TIMEOUT_SECONDS,
		fastConnectOpenTimeoutSeconds = FAST_CONNECT_OPEN_TIMEOUT_SECONDS,
		connectSessionTimeoutSeconds = CONNECT_SESSION_TIMEOUT_SECONDS,
		nextRunCloseDelaySeconds = NEXT_RUN_CLOSE_DELAY_SECONDS,
		nextRunReconnectDelaySeconds = NEXT_RUN_RECONNECT_DELAY_SECONDS,
		nextRunConnectTimeoutSeconds = NEXT_RUN_CONNECT_TIMEOUT_SECONDS,
		nextRunFastWindowSeconds = NEXT_RUN_FAST_WINDOW_SECONDS,
		debugBridgeConnection = DEBUG_BRIDGE_CONNECTION,
		bridgeVersion = BRIDGE_VERSION,
		protocolVersion = BRIDGE_PROTOCOL_VERSION,
		codecVersion = CODEC_VERSION,
		bridgeBuildUnix = BRIDGE_BUILD_UNIX,
		chunkFrameProtocolVersion = CHUNK_FRAME_PROTOCOL_VERSION,
		compactValueProtocolVersion = COMPACT_VALUE_PROTOCOL_VERSION,
		preSerializeLargeServiceWarm = PRE_SERIALIZE_LARGE_SERVICE_WARM,
		serializerWorkerMode = SERIALIZER_WORKER_MODE,
		maxRequestBytes = 16 * 1024 * 1024,
		maxQueuedExclusiveRequests = 16,
		allowedMethods = BRIDGE_ALLOWED_METHODS,
		isExclusiveMethod = function(method)
			return BRIDGE_EXCLUSIVE_METHODS[method] == true
		end,
		handleMethod = Config.handleMethod,
		updateStatusText = Config.updateStatusText,
		onRuntimeSettingsChanged = Config.applyBridgeRuntimeSettings,
	})
end

return BridgePluginRuntime
