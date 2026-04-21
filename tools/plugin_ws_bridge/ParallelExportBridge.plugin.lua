--!nocheck
-- Studio plugin: multi-channel WebSocket bridge for fast export RPCs.
-- Import this script as a plugin script.

local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")

if not plugin then
	error("ParallelExportBridge.plugin.lua must run as a Studio plugin")
end

local SETTINGS_PREFIX = "ParallelExportBridge."
local DEFAULT_HOST = "127.0.0.1"
local DEFAULT_PORTS = { 8781, 8782, 8783, 8784 }
local RECONNECT_SECONDS = 0.2
local UI = {}
local Config = {}

local function requireChildModule(name: string): any
	local child = script:FindFirstChild(name)
	if child and child:IsA("ModuleScript") then
		local ok, result = pcall(require, child)
		if ok and type(result) == "table" then
			return result
		end
		error("[ParallelExportBridge] failed to require module " .. name .. ": " .. tostring(result))
	end
	error("[ParallelExportBridge] missing child ModuleScript: " .. name)
end

local function tryRequireChildModule(name: string): any?
	local child = script:FindFirstChild(name)
	if child and child:IsA("ModuleScript") then
		local ok, result = pcall(require, child)
		if ok and type(result) == "table" then
			return result
		end
		warn("[ParallelExportBridge] failed optional module " .. name .. ": " .. tostring(result))
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
		warn("[ParallelExportBridge] failed optional nested module " .. name .. ": " .. tostring(result))
	end
	return nil
end

local SettingsModule = requireChildModule("BridgeSettings")
local ThemeModule = requireChildModule("BridgeTheme")
local StatusModule = requireChildModule("BridgeStatus")
local _ = tryRequireChildModule("RbxDom")
local RbxDomDatabase = tryRequireNestedModule(script:FindFirstChild("RbxDom"), "database")

local toolbar = plugin:CreateToolbar("Parallel Export Bridge")
local openButton = toolbar:CreateButton(
	"Open UI",
	"Open Parallel Export Bridge panel",
	"rbxassetid://4458901886"
)
local toggleButton = toolbar:CreateButton(
	"Bridge Enabled",
	"Enable or disable Parallel Export Bridge connections",
	"rbxassetid://4458901886"
)
local reconnectButton = toolbar:CreateButton(
	"Reconnect",
	"Reconnect all WebSocket channels",
	"rbxassetid://4458901886"
)

local statusWidgetInfo = DockWidgetPluginGuiInfo.new(
	Enum.InitialDockState.Right,
	false,
	false,
	360,
	230,
	320,
	140
)
local statusWidget: DockWidgetPluginGui
do
	local ok, widget = pcall(function()
		return plugin:CreateDockWidgetPluginGuiAsync("ParallelExportBridgeStatus", statusWidgetInfo)
	end)
	if ok and widget then
		statusWidget = widget
	else
		statusWidget = plugin:CreateDockWidgetPluginGui("ParallelExportBridgeStatus", statusWidgetInfo)
	end
end
statusWidget.Title = "Parallel Export Bridge"
statusWidget.Enabled = false

function UI.showWidget()
	statusWidget.Enabled = true
	pcall(function()
		statusWidget:RequestRaise()
	end)
end

local rootFrame = Instance.new("Frame")
rootFrame.Size = UDim2.fromScale(1, 1)
rootFrame.BackgroundColor3 = Color3.fromRGB(28, 28, 28)
rootFrame.BorderSizePixel = 0
rootFrame.Parent = statusWidget

local controlsFrame = Instance.new("Frame")
controlsFrame.Size = UDim2.new(1, -12, 0, 128)
controlsFrame.Position = UDim2.new(0, 6, 0, 6)
controlsFrame.BackgroundTransparency = 1
controlsFrame.Parent = rootFrame

local hostLabel = Instance.new("TextLabel")
hostLabel.Size = UDim2.new(0, 42, 0, 24)
hostLabel.Position = UDim2.new(0, 0, 0, 0)
hostLabel.BackgroundTransparency = 1
hostLabel.Text = "Host"
hostLabel.TextXAlignment = Enum.TextXAlignment.Left
hostLabel.Font = Enum.Font.Code
hostLabel.TextSize = 13
hostLabel.TextColor3 = Color3.fromRGB(220, 220, 220)
hostLabel.Parent = controlsFrame

local hostBox = Instance.new("TextBox")
hostBox.Size = UDim2.new(1, -48, 0, 24)
hostBox.Position = UDim2.new(0, 48, 0, 0)
hostBox.BackgroundColor3 = Color3.fromRGB(45, 45, 45)
hostBox.TextColor3 = Color3.fromRGB(240, 240, 240)
hostBox.PlaceholderText = "127.0.0.1"
hostBox.ClearTextOnFocus = false
hostBox.Font = Enum.Font.Code
hostBox.TextSize = 13
hostBox.Parent = controlsFrame

local portsLabel = Instance.new("TextLabel")
portsLabel.Size = UDim2.new(0, 42, 0, 24)
portsLabel.Position = UDim2.new(0, 0, 0, 30)
portsLabel.BackgroundTransparency = 1
portsLabel.Text = "Ports"
portsLabel.TextXAlignment = Enum.TextXAlignment.Left
portsLabel.Font = Enum.Font.Code
portsLabel.TextSize = 13
portsLabel.TextColor3 = Color3.fromRGB(220, 220, 220)
portsLabel.Parent = controlsFrame

local portsBox = Instance.new("TextBox")
portsBox.Size = UDim2.new(1, -48, 0, 24)
portsBox.Position = UDim2.new(0, 48, 0, 30)
portsBox.BackgroundColor3 = Color3.fromRGB(45, 45, 45)
portsBox.TextColor3 = Color3.fromRGB(240, 240, 240)
portsBox.PlaceholderText = "8781,8782,8783,8784"
portsBox.ClearTextOnFocus = false
portsBox.Font = Enum.Font.Code
portsBox.TextSize = 13
portsBox.Parent = controlsFrame

local exportAllButton = Instance.new("TextButton")
exportAllButton.Size = UDim2.new(0, 180, 0, 28)
exportAllButton.Position = UDim2.new(0, 0, 0, 64)
exportAllButton.BackgroundColor3 = Color3.fromRGB(52, 52, 52)
exportAllButton.TextColor3 = Color3.fromRGB(240, 240, 240)
exportAllButton.Font = Enum.Font.Code
exportAllButton.TextSize = 13
exportAllButton.Text = "Export All Properties: ON"
exportAllButton.AutoButtonColor = false
exportAllButton.Active = false
exportAllButton.Parent = controlsFrame

local preSerializeButton = Instance.new("TextButton")
preSerializeButton.Size = UDim2.new(0, 160, 0, 28)
preSerializeButton.Position = UDim2.new(1, -160, 0, 64)
preSerializeButton.BackgroundColor3 = Color3.fromRGB(52, 52, 52)
preSerializeButton.TextColor3 = Color3.fromRGB(240, 240, 240)
preSerializeButton.Font = Enum.Font.Code
preSerializeButton.TextSize = 13
preSerializeButton.Text = "Pre-Serialize: ON"
preSerializeButton.Parent = controlsFrame

local applyButton = Instance.new("TextButton")
applyButton.Size = UDim2.new(0, 120, 0, 28)
applyButton.Position = UDim2.new(1, -120, 0, 96)
applyButton.BackgroundColor3 = Color3.fromRGB(35, 95, 55)
applyButton.TextColor3 = Color3.fromRGB(255, 255, 255)
applyButton.Font = Enum.Font.Code
applyButton.TextSize = 13
applyButton.Text = "Apply + Reconnect"
applyButton.Parent = controlsFrame

local enabledButton = Instance.new("TextButton")
enabledButton.Size = UDim2.new(0, 140, 0, 28)
enabledButton.Position = UDim2.new(0, 0, 0, 96)
enabledButton.BackgroundColor3 = Color3.fromRGB(95, 35, 35)
enabledButton.TextColor3 = Color3.fromRGB(255, 255, 255)
enabledButton.Font = Enum.Font.Code
enabledButton.TextSize = 13
enabledButton.Text = "Bridge: OFF"
enabledButton.Parent = controlsFrame

local statusLabel = Instance.new("TextLabel")
statusLabel.Size = UDim2.new(1, -12, 1, -142)
statusLabel.Position = UDim2.new(0, 6, 0, 136)
statusLabel.BackgroundTransparency = 1
statusLabel.TextXAlignment = Enum.TextXAlignment.Left
statusLabel.TextYAlignment = Enum.TextYAlignment.Top
statusLabel.Font = Enum.Font.Code
statusLabel.TextSize = 13
statusLabel.TextWrapped = false
statusLabel.Text = "Starting..."
statusLabel.TextColor3 = Color3.fromRGB(210, 210, 210)
statusLabel.Parent = rootFrame

function UI.applyStudioTheme()
	local studio = settings().Studio
	local theme = studio.Theme
	ThemeModule.apply(theme, {
		rootFrame = rootFrame,
		hostLabel = hostLabel,
		portsLabel = portsLabel,
		hostBox = hostBox,
		portsBox = portsBox,
		exportAllButton = exportAllButton,
		preSerializeButton = preSerializeButton,
		applyButton = applyButton,
		enabledButton = enabledButton,
		statusLabel = statusLabel,
	})
end

UI.applyStudioTheme()
settings().Studio.ThemeChanged:Connect(UI.applyStudioTheme)

local host: string = SettingsModule.loadHost(plugin, SETTINGS_PREFIX, DEFAULT_HOST)
local ports: { number } = SettingsModule.loadPorts(plugin, SETTINGS_PREFIX, DEFAULT_PORTS)
local BRIDGE_ENABLED: boolean = SettingsModule.loadEnabled(plugin, SETTINGS_PREFIX, false)

hostBox.Text = host
portsBox.Text = table.concat(ports, ",")

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
}

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

local SUPPORTED_RBX_DOM_VALUE_TYPES = {
	Bool = true,
	Int32 = true,
	Int64 = true,
	Float32 = true,
	Float64 = true,
	String = true,
	ContentId = true,
	Ref = true,
	Vector2 = true,
	Vector3 = true,
	UDim = true,
	UDim2 = true,
	Color3 = true,
	Color3uint8 = true,
	ColorSequence = true,
	NumberSequence = true,
	CFrame = true,
	Rect = true,
	Font = true,
	BrickColor = true,
}

local function isRbxDomPropertyReadable(propertyData: any): boolean
	local scriptability = propertyData and propertyData.Scriptability
	return scriptability == "Read" or scriptability == "ReadWrite" or scriptability == "Custom"
end

local function isRbxDomPropertySerializable(propertyData: any): boolean
	local kind = propertyData and propertyData.Kind
	if type(kind) ~= "table" then
		return false
	end

	if type(kind.Alias) == "table" then
		return false
	end

	local canonical = kind.Canonical
	if type(canonical) ~= "table" then
		return false
	end

	local serialization = canonical.Serialization
	if serialization == nil then
		return true
	end
	if type(serialization) == "string" then
		return serialization ~= "DoesNotSerialize"
	end
	return true
end

local function isRbxDomPropertyTypeSupported(propertyData: any): boolean
	local dataType = propertyData and propertyData.DataType
	if type(dataType) ~= "table" then
		return false
	end

	if type(dataType.Enum) == "string" then
		return true
	end

	local valueType = dataType.Value
	if type(valueType) ~= "string" then
		return false
	end

	return SUPPORTED_RBX_DOM_VALUE_TYPES[valueType] == true
end

local function buildPropertyCandidatesFromRbxDom(database: any): { [string]: { string } }
	local byClass: { [string]: { string } } = {}
	if type(database) ~= "table" then
		return byClass
	end

	local classes = database.Classes
	if type(classes) ~= "table" then
		return byClass
	end

	local memo: { [string]: { string } } = {}
	local visiting: { [string]: boolean } = {}

	local function collectNamesForClass(className: string): { string }
		local cached = memo[className]
		if cached then
			return cached
		end

		if visiting[className] then
			return {}
		end
		visiting[className] = true

		local names = {}
		local seen: { [string]: boolean } = {}

		local classData = classes[className]
		if type(classData) == "table" then
			local superclass = classData.Superclass
			if type(superclass) == "string" and superclass ~= "" then
				local inherited = collectNamesForClass(superclass)
				for _, inheritedName in ipairs(inherited) do
					local inheritedKey = string.lower(inheritedName)
					if not seen[inheritedKey] then
						seen[inheritedKey] = true
						names[#names + 1] = inheritedName
					end
				end
			end

			local properties = classData.Properties
			if type(properties) == "table" then
				for propertyName, propertyData in pairs(properties) do
					if type(propertyName) == "string" then
						local lowered = string.lower(propertyName)
						if
							lowered ~= "source"
							and lowered ~= "robloxlocked"
							and isRbxDomPropertyReadable(propertyData)
							and isRbxDomPropertySerializable(propertyData)
							and isRbxDomPropertyTypeSupported(propertyData)
							and not seen[lowered]
						then
							seen[lowered] = true
							names[#names + 1] = propertyName
						end
					end
				end
			end
		end

		table.sort(names)
		visiting[className] = nil
		memo[className] = names
		return names
	end

	for className, classData in pairs(classes) do
		if type(className) == "string" and type(classData) == "table" then
			local names = collectNamesForClass(className)
			if #names > 0 then
				byClass[className] = names
			end
		end
	end

	return byClass
end

local function countPropertyCandidates(byClass: { [string]: { string } }): (number, number)
	local classCount = 0
	local propertyCount = 0
	for _, names in pairs(byClass) do
		classCount += 1
		propertyCount += #names
	end
	return classCount, propertyCount
end

local NO_DEFAULTS = {}
local NO_PROPERTIES = {}
local DEFAULT_PROPERTY_CACHE: { [string]: any } = {}
local CLASS_PROPERTY_CANDIDATES_CACHE: { [string]: any } = {}
local EXPORT_ALL_PROPERTIES = true
local configuredPreSerialize = plugin:GetSetting(SETTINGS_PREFIX .. "preSerialize")
local PRE_SERIALIZE_ON_PREPARE = configuredPreSerialize ~= false
local PRE_SERIALIZE_INSTANCE_THRESHOLD = 5000
local EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS: { [string]: { string } } = buildPropertyCandidatesFromRbxDom(RbxDomDatabase)
do
	local classCount, propertyCount = countPropertyCandidates(EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS)
	if classCount > 0 then
		print(
			("[ParallelExportBridge] loaded bundled rbx-dom property candidates: classes=%d, properties=%d"):format(
				classCount,
				propertyCount
			)
		)
	end
end

exportAllButton.Text = "Export All Properties: ON (Locked)"
preSerializeButton.Text = ("Pre-Serialize: %s (<=%d)"):format(
	PRE_SERIALIZE_ON_PREPARE and "ON" or "OFF",
	PRE_SERIALIZE_INSTANCE_THRESHOLD
)

type ServiceState = {
	instances: { Instance },
	classNames: { string },
	generatedAtUnix: number,
	rootName: string,
	rootClassName: string,
	rootPath: string,
	pathByInstance: { [Instance]: string },
	debugIdByInstance: { [Instance]: string | boolean },
	instanceIdByInstance: { [Instance]: string | boolean },
	scriptObjects: { LuaSourceContainer },
	scriptPaths: { string }?,
	scriptSources: { [string]: string },
	scriptInstances: { [string]: LuaSourceContainer }?,
	scriptKeyByInstance: { [Instance]: string },
	classDefaults: { [string]: any }?,
	classDefaultsEncoded: string?,
	serializedInstances: { [number]: any }?,
	scriptPathsEncoded: string?,
	sourceBatchEncodedByKey: { [string]: string },
	batchCacheByKey: { [string]: string },
	batchCacheKeys: { string },
	safeReadByClass: { [string]: boolean },
}

local stateByService: { [string]: ServiceState } = {}

local channels: {
	[number]: {
		id: number,
		port: number,
		client: WebStreamClient?,
		open: boolean,
		connecting: boolean,
		reconnectScheduled: boolean,
		shouldReconnect: boolean,
	},
} = {}

function UI.updateStatusText()
	statusLabel.Text = StatusModule.render({
		enabled = BRIDGE_ENABLED,
		host = host,
		ports = ports,
		exportAllProperties = EXPORT_ALL_PROPERTIES,
		preSerializeOnPrepare = PRE_SERIALIZE_ON_PREPARE,
		preSerializeInstanceThreshold = PRE_SERIALIZE_INSTANCE_THRESHOLD,
		channels = channels,
	})
end

function UI.updateEnabledState()
	enabledButton.Text = ("Bridge: %s"):format(BRIDGE_ENABLED and "ON" or "OFF")
	enabledButton.BackgroundColor3 = BRIDGE_ENABLED and Color3.fromRGB(35, 95, 55)
		or Color3.fromRGB(95, 35, 35)
	toggleButton:SetActive(BRIDGE_ENABLED)
end

local function tryRead(instance: Instance, propertyName: string): (boolean, any)
	return pcall(function()
		return (instance :: any)[propertyName]
	end)
end

local function getDebugId(instance: Instance): string?
	local ok, debugId = pcall(function()
		return instance:GetDebugId(32)
	end)
	if ok and type(debugId) == "string" and #debugId > 0 then
		return debugId
	end
	return nil
end

local function hashString32(text: string, seed: number): number
	local hash = seed % 4294967296
	for i = 1, #text do
		local byte = string.byte(text, i)
		hash = (hash * 33 + byte) % 4294967296
	end
	return hash
end

local function shortenIdentifier(raw: string): string
	local h1 = hashString32(raw, 5381)
	local h2 = hashString32(raw, 2166136261)
	return string.format("%08x%08x", h1, h2)
end

local function tryIsPropertyModified(instance: Instance, propertyName: string): (boolean, boolean?)
	local ok, modified = pcall(function()
		return instance:IsPropertyModified(propertyName)
	end)
	if ok and type(modified) == "boolean" then
		return true, modified
	end
	return false, nil
end

local function getRefPathSegments(instance: Instance): { string }?
	if instance == game then
		return {}
	end
	if not instance:IsDescendantOf(game) then
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

local function serializeRefValue(state: ServiceState?, instance: Instance): any
	local pathSegments = getRefPathSegments(instance)
	if pathSegments == nil or #pathSegments == 0 then
		return nil
	end

	local out = {
		_type = "Ref",
		pathSegments = pathSegments,
	}

	if state ~= nil then
		local cachedInstanceId = state.instanceIdByInstance[instance]
		if type(cachedInstanceId) == "string" and #cachedInstanceId > 0 then
			out.instanceId = cachedInstanceId
		end
		local cachedDebugId = state.debugIdByInstance[instance]
		if type(cachedDebugId) == "string" and #cachedDebugId > 0 then
			out.debugId = cachedDebugId
		else
			local debugId = getDebugId(instance)
			if debugId ~= nil and #debugId > 0 then
				out.debugId = debugId
			end
		end
	else
		local debugId = getDebugId(instance)
		if debugId ~= nil and #debugId > 0 then
			out.debugId = debugId
		end
	end

	return out
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
	elseif valueType == "Instance" then
		return serializeRefValue(state, value)
	end
	return nil
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
		if propertyName ~= "Source" then
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

local function configurePropertyCandidates(payload: any): { [string]: any }
	if type(payload) ~= "table" then
		error("configurePropertyCandidates expects table payload")
	end

	local function normalizePropertyName(name: string): string
		return string.match(name, "^%s*(.-)%s*$")
	end

	local function propertyKey(name: string): string
		return string.lower(name)
	end

	local function shouldSkipProperty(name: string): boolean
		local key = propertyKey(name)
		return key == "source" or key == "robloxlocked"
	end

	EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS = {}
	DEFAULT_PROPERTY_CACHE = {}
	CLASS_PROPERTY_CANDIDATES_CACHE = {}
	for serviceName, _ in pairs(stateByService) do
		stateByService[serviceName] = nil
	end

	local classCount = 0
	local propertyCount = 0
	for className, names in pairs(payload) do
		if type(className) == "string" and type(names) == "table" then
			local sanitized = {}
			local seen: { [string]: boolean } = {}
			for _, name in ipairs(names) do
				if type(name) == "string" then
					local normalized = normalizePropertyName(name)
					local key = propertyKey(normalized)
					if normalized ~= "" and not shouldSkipProperty(normalized) and not seen[key] then
						seen[key] = true
						sanitized[#sanitized + 1] = normalized
					end
				end
			end
			if #sanitized > 0 then
				EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS[className] = sanitized
				classCount += 1
				propertyCount += #sanitized
			end
		end
	end

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
	plugin:SetSetting(SETTINGS_PREFIX .. "exportAllProperties", true)
	plugin:SetSetting(SETTINGS_PREFIX .. "preSerialize", PRE_SERIALIZE_ON_PREPARE)
	exportAllButton.Text = "Export All Properties: ON (Locked)"
	preSerializeButton.Text = ("Pre-Serialize: %s (<=%d)"):format(
		PRE_SERIALIZE_ON_PREPARE and "ON" or "OFF",
		PRE_SERIALIZE_INSTANCE_THRESHOLD
	)
	UI.updateStatusText()
	return {
		exportAllProperties = true,
		preSerializeOnPrepare = PRE_SERIALIZE_ON_PREPARE,
		preSerializeInstanceThreshold = PRE_SERIALIZE_INSTANCE_THRESHOLD,
	}
end

local function getClassPropertyCandidates(className: string): { string }?
	local cached = CLASS_PROPERTY_CANDIDATES_CACHE[className]
	if cached == nil then
		local external = EXTERNAL_PROPERTY_CANDIDATES_BY_CLASS[className]
		if external ~= nil and #external > 0 then
			local ok, probe = pcall(function()
				return Instance.new(className)
			end)
			if ok and probe ~= nil then
				local validated = {}
				for _, propertyName in ipairs(external) do
					if propertyName ~= "Source" then
						local readable = tryRead(probe, propertyName)
						if readable then
							validated[#validated + 1] = propertyName
						end
					end
				end
				probe:Destroy()
				if #validated > 0 then
					CLASS_PROPERTY_CANDIDATES_CACHE[className] = validated
					cached = validated
				else
					CLASS_PROPERTY_CANDIDATES_CACHE[className] = NO_PROPERTIES
					cached = NO_PROPERTIES
				end
			else
				CLASS_PROPERTY_CANDIDATES_CACHE[className] = external
				cached = external
			end
		elseif EXPORT_ALL_PROPERTIES then
			CLASS_PROPERTY_CANDIDATES_CACHE[className] = NO_PROPERTIES
			cached = NO_PROPERTIES
		else
			getDefaultSerializedProperties(className)
			cached = CLASS_PROPERTY_CANDIDATES_CACHE[className]
		end
	end
	if cached == nil or cached == NO_PROPERTIES then
		return nil
	end
	return cached
end

local function getCachedInstancePath(state: ServiceState, instance: Instance): string
	local cached = state.pathByInstance[instance]
	if cached then
		return cached
	end
	local parent = instance.Parent
	local path: string
	if parent == nil or parent == game then
		path = instance.Name
	else
		path = getCachedInstancePath(state, parent) .. "." .. instance.Name
	end
	state.pathByInstance[instance] = path
	return path
end

local function getCachedParentPath(state: ServiceState, instance: Instance): string?
	local parent = instance.Parent
	if parent == nil then
		return nil
	end
	if parent == game then
		return "game"
	end
	return getCachedInstancePath(state, parent)
end

local function getCachedDebugId(state: ServiceState, instance: Instance): string?
	local cached = state.debugIdByInstance[instance]
	if cached ~= nil then
		if cached == false then
			return nil
		end
		return cached :: string
	end

	local debugId = getDebugId(instance)
	if debugId then
		state.debugIdByInstance[instance] = debugId
		return debugId
	end
	state.debugIdByInstance[instance] = false
	return nil
end

local function getCachedInstanceId(state: ServiceState, instance: Instance): string?
	local cached = state.instanceIdByInstance[instance]
	if cached ~= nil then
		if cached == false then
			return nil
		end
		return cached :: string
	end
	state.instanceIdByInstance[instance] = false
	return nil
end

local function getCachedParentDebugId(state: ServiceState, instance: Instance): string?
	local parent = instance.Parent
	if parent == nil or parent == game then
		return nil
	end
	return getCachedDebugId(state, parent)
end

local function getCachedParentInstanceId(state: ServiceState, instance: Instance): string?
	local parent = instance.Parent
	if parent == nil or parent == game then
		return nil
	end
	return getCachedInstanceId(state, parent)
end

local function getCachedScriptSourceKey(state: ServiceState, instance: LuaSourceContainer): string
	local cached = state.scriptKeyByInstance[instance]
	if cached ~= nil then
		return cached
	end

	local key: string
	local instanceId = getCachedInstanceId(state, instance)
	if instanceId ~= nil and #instanceId > 0 then
		key = "id:" .. instanceId
	else
		key = "path:" .. getCachedInstancePath(state, instance)
	end

	state.scriptKeyByInstance[instance] = key
	return key
end

local function ensureScriptIndex(state: ServiceState)
	if state.scriptPaths and state.scriptInstances then
		return
	end
	local scriptPaths = table.create(#state.scriptObjects)
	local scriptInstances = {}
	for i, inst in ipairs(state.scriptObjects) do
		local sourceKey = getCachedScriptSourceKey(state, inst)
		scriptPaths[i] = sourceKey
		scriptInstances[sourceKey] = inst
	end
	table.sort(scriptPaths)
	state.scriptPaths = scriptPaths
	state.scriptInstances = scriptInstances
	state.scriptPathsEncoded = nil
end

local function chunkEncodedString(encoded: string, startIndex: number?, maxLen: number?): { [string]: any }
	local total = #encoded
	local startPos = math.max(1, startIndex or 1)
	local take = math.max(1, maxLen or 2000)
	if startPos > total then
		return { start = startPos, nextStart = startPos, total = total, chunk = "" }
	end
	local finish = math.min(total, startPos + take - 1)
	return {
		start = startPos,
		nextStart = finish + 1,
		total = total,
		chunk = string.sub(encoded, startPos, finish),
	}
end

local function exportInstanceInternal(
	state: ServiceState,
	instance: Instance,
	safeReads: boolean,
	path: string,
	parentPath: string?,
	debugId: string?,
	parentDebugId: string?,
	instanceId: string?,
	parentInstanceId: string?
): { [string]: any }
	local entry: { [string]: any } = {
		name = instance.Name,
		className = instance.ClassName,
		path = path,
		pathSegments = getRefPathSegments(instance),
		parentPath = parentPath,
		parentDebugId = parentDebugId,
		parentInstanceId = parentInstanceId,
		attributes = instance:GetAttributes(),
	}
	local properties = {}
	local defaultProperties = nil
	if not EXPORT_ALL_PROPERTIES then
		defaultProperties = getDefaultSerializedProperties(instance.ClassName)
	end

	if debugId then
		entry.debugId = debugId
	end
	if instanceId then
		entry.instanceId = instanceId
	end

	local propertyNames = getClassPropertyCandidates(instance.ClassName) or PROPERTY_CANDIDATES
	for _, propertyName in ipairs(propertyNames) do
		if propertyName ~= "Source" then
			local defaultSerialized = defaultProperties and defaultProperties[propertyName] or nil
			local skipRead = false
			local hasModifiedState = false
			if not EXPORT_ALL_PROPERTIES then
				local hasModified, isModified = tryIsPropertyModified(instance, propertyName)
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
							properties[propertyName] = serialized
						elseif defaultSerialized == nil or not deepEqual(serialized, defaultSerialized) then
							properties[propertyName] = serialized
						end
					end
				end
			end
		end
	end

	if instance:IsA("LuaSourceContainer") then
		properties.Source = "__SOURCE_EXTERNAL__"
		entry.sourceKey = getCachedScriptSourceKey(state, instance)
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
	parentInstanceId: string?
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
		parentInstanceId
	)
end

local function exportInstanceSafe(
	state: ServiceState,
	instance: Instance,
	path: string,
	parentPath: string?,
	debugId: string?,
	parentDebugId: string?,
	instanceId: string?,
	parentInstanceId: string?
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
		parentInstanceId
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
	parentInstanceId: string?
): { [string]: any }
	local className = instance.ClassName
	if state.safeReadByClass[className] then
		return exportInstanceSafe(
			state,
			instance,
			path,
			parentPath,
			debugId,
			parentDebugId,
			instanceId,
			parentInstanceId
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
		parentInstanceId
	)
	if ok then
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
		parentInstanceId
	)
end

local function prepareService(serviceName: string): { [string]: any }
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
	local pathByInstance = { [service] = service.Name }
	local debugIdByInstance: { [Instance]: string | boolean } = {}
	local instanceIdByInstance: { [Instance]: string | boolean } = {}
	local scriptKeyByInstance: { [Instance]: string } = {}
	local serviceDebugId = getDebugId(service)
	if serviceDebugId then
		debugIdByInstance[service] = serviceDebugId
	else
		debugIdByInstance[service] = false
	end
	instanceIdByInstance[service] = string.format("%x", 1)

	local serviceClassName = service.ClassName
	classSeen[serviceClassName] = true
	classNames[1] = serviceClassName

	if service:IsA("LuaSourceContainer") then
		scriptCount += 1
		scriptObjects[scriptCount] = service
	end

	for _, inst in ipairs(descendants) do
		instanceCount += 1
		instances[instanceCount] = inst
		local debugId = getDebugId(inst)
		if debugId then
			debugIdByInstance[inst] = debugId
		else
			debugIdByInstance[inst] = false
		end
		instanceIdByInstance[inst] = string.format("%x", instanceCount)

		local className = inst.ClassName
		if not classSeen[className] then
			classSeen[className] = true
			classNames[#classNames + 1] = className
		end

		if inst:IsA("LuaSourceContainer") then
			scriptCount += 1
			scriptObjects[scriptCount] = inst
		end
	end

	stateByService[serviceName] = {
		instances = instances,
		classNames = classNames,
		generatedAtUnix = os.time(),
		rootName = service.Name,
		rootClassName = service.ClassName,
		rootPath = service.Name,
		pathByInstance = pathByInstance,
		debugIdByInstance = debugIdByInstance,
		instanceIdByInstance = instanceIdByInstance,
		scriptObjects = scriptObjects,
		scriptPaths = nil,
		scriptSources = {},
		scriptInstances = nil,
		scriptKeyByInstance = scriptKeyByInstance,
		classDefaults = nil,
		classDefaultsEncoded = nil,
		serializedInstances = nil,
		scriptPathsEncoded = nil,
		sourceBatchEncodedByKey = {},
		batchCacheByKey = {},
		batchCacheKeys = {},
		safeReadByClass = {},
	}

	local state = stateByService[serviceName]
	local preSerialized = PRE_SERIALIZE_ON_PREPARE and instanceCount <= PRE_SERIALIZE_INSTANCE_THRESHOLD
	if preSerialized then
		local serialized = table.create(instanceCount)
		for i, inst in ipairs(instances) do
			local path = getCachedInstancePath(state, inst)
			local parentPath = getCachedParentPath(state, inst)
			local debugId = getCachedDebugId(state, inst)
			local parentDebugId = getCachedParentDebugId(state, inst)
			local instanceId = getCachedInstanceId(state, inst)
			local parentInstanceId = getCachedParentInstanceId(state, inst)
			local entry = exportInstanceWithFallback(
				state,
				inst,
				path,
				parentPath,
				debugId,
				parentDebugId,
				instanceId,
				parentInstanceId
			)
			serialized[i] = entry
		end
		state.serializedInstances = serialized
	end

	return {
		service = serviceName,
		generatedAtUnix = state.generatedAtUnix,
		rootName = state.rootName,
		rootClassName = state.rootClassName,
		rootPath = state.rootPath,
		instanceCount = instanceCount,
		scriptCount = scriptCount,
		preSerialized = preSerialized,
		preSerializeInstanceThreshold = PRE_SERIALIZE_INSTANCE_THRESHOLD,
	}
end

local function getState(serviceName: string): ServiceState
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

local function getInstanceBatch(serviceName: string, startIndex: number?, maxCount: number?): string
	local state = getState(serviceName)
	local key = tostring(startIndex or 1) .. ":" .. tostring(maxCount or 300)
	local cachedPayload = state.batchCacheByKey[key]
	if cachedPayload then
		return cachedPayload
	end

	local instances = state.instances
	local total = #instances
	local startPos = math.max(1, startIndex or 1)
	local take = math.max(1, maxCount or 300)
	local encoded: string

	if startPos > total then
		encoded = HttpService:JSONEncode({ start = startPos, nextStart = startPos, total = total, items = {} })
	else
		local finish = math.min(total, startPos + take - 1)
		local items = table.create(finish - startPos + 1)
		if state.serializedInstances then
			for i = startPos, finish do
				items[#items + 1] = state.serializedInstances[i]
			end
		else
			for i = startPos, finish do
				local inst = instances[i]
				local path = getCachedInstancePath(state, inst)
				local parentPath = getCachedParentPath(state, inst)
				local debugId = getCachedDebugId(state, inst)
				local parentDebugId = getCachedParentDebugId(state, inst)
				local instanceId = getCachedInstanceId(state, inst)
				local parentInstanceId = getCachedParentInstanceId(state, inst)
				local entry = exportInstanceWithFallback(
					state,
					inst,
					path,
					parentPath,
					debugId,
					parentDebugId,
					instanceId,
					parentInstanceId
				)
				items[#items + 1] = entry
			end
		end
		encoded = HttpService:JSONEncode({
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
	return encoded
end

local function getClassDefaults(serviceName: string): string
	local state = getState(serviceName)
	if state.classDefaultsEncoded then
		return state.classDefaultsEncoded
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
	state.classDefaultsEncoded = HttpService:JSONEncode(state.classDefaults)
	return state.classDefaultsEncoded
end

local function getScriptPaths(serviceName: string): string
	local state = getState(serviceName)
	ensureScriptIndex(state)
	if not state.scriptPathsEncoded then
		state.scriptPathsEncoded = HttpService:JSONEncode(state.scriptPaths)
	end
	return state.scriptPathsEncoded
end

local function getOrLoadScriptSource(state: ServiceState, instancePath: string): string
	local src = state.scriptSources[instancePath]
	if src ~= nil then
		return src
	end

	local scriptInstance = state.scriptInstances and state.scriptInstances[instancePath] or nil
	if scriptInstance == nil then
		return ""
	end

	local ok, loaded = pcall(function()
		return scriptInstance.Source
	end)
	src = ok and loaded or ""
	state.scriptSources[instancePath] = src
	return src
end

local function getSourceChunk(serviceName: string, instancePath: string, startIndex: number?, maxLen: number?): { [string]: any }
	local state = getState(serviceName)
	ensureScriptIndex(state)
	local src = getOrLoadScriptSource(state, instancePath)
	return chunkEncodedString(src, startIndex, maxLen)
end

local function getSourceBatchChunk(
	serviceName: string,
	instancePaths: { string },
	startIndex: number?,
	maxLen: number?
): { [string]: any }
	local state = getState(serviceName)
	ensureScriptIndex(state)
	local cacheKey = table.concat(instancePaths, "\n")
	local encoded = state.sourceBatchEncodedByKey[cacheKey]
	if not encoded then
		local entries = table.create(#instancePaths)
		for i, instancePath in ipairs(instancePaths) do
			entries[i] = {
				instancePath = instancePath,
				source = getOrLoadScriptSource(state, instancePath),
			}
		end
		encoded = HttpService:JSONEncode(entries)
		state.sourceBatchEncodedByKey[cacheKey] = encoded
	end
	return chunkEncodedString(encoded, startIndex, maxLen)
end

local perfState

local function handleMethod(method: string, params: { [string]: any }?): any
	local p = params or {}
	if method == "ping" then
		return { ok = true, timestamp = os.time() }
	elseif method == "getPerformanceStats" then
		return {
			fps = perfState.fps,
			frameMs = perfState.frameMs,
			lastHeartbeat = perfState.lastHeartbeat,
			sampleCount = perfState.sampleCount,
		}
	elseif method == "configurePropertyCandidates" then
		return configurePropertyCandidates(p.classes)
	elseif method == "setExportOptions" then
		return configureExportOptions(p)
	elseif method == "prepare" then
		return prepareService(tostring(p.service))
	elseif method == "getInstanceBatchChunk" then
		local encoded = getInstanceBatch(tostring(p.service), p.startIndex, p.maxCount)
		return chunkEncodedString(encoded, p.chunkStart, p.maxLen)
	elseif method == "getClassDefaultsChunk" then
		local encoded = getClassDefaults(tostring(p.service))
		return chunkEncodedString(encoded, p.startIndex, p.maxLen)
	elseif method == "getScriptPathsChunk" then
		local encoded = getScriptPaths(tostring(p.service))
		return chunkEncodedString(encoded, p.startIndex, p.maxLen)
	elseif method == "getSourceChunk" then
		return getSourceChunk(tostring(p.service), tostring(p.instancePath), p.startIndex, p.maxLen)
	elseif method == "getSourceBatchChunk" then
		return getSourceBatchChunk(
			tostring(p.service),
			type(p.instancePaths) == "table" and p.instancePaths or {},
			p.startIndex,
			p.maxLen
		)
	elseif method == "release" then
		stateByService[tostring(p.service)] = nil
		return "ok"
	else
		error("Unknown method: " .. tostring(method))
	end
end

local function sendEnvelope(client: WebStreamClient, envelope: { [string]: any })
	local ok, encoded = pcall(function()
		return HttpService:JSONEncode(envelope)
	end)
	if ok then
		client:Send(encoded)
	end
end

perfState = {
	fps = 60.0,
	frameMs = 16.67,
	lastHeartbeat = os.clock(),
	sampleCount = 0,
}

RunService.Heartbeat:Connect(function(dt: number)
	if dt <= 0 then
		return
	end
	local instantFps = 1 / dt
	local alpha = 0.08
	perfState.fps = perfState.fps + (instantFps - perfState.fps) * alpha
	if perfState.fps <= 0 then
		perfState.fps = instantFps
	end
	perfState.frameMs = 1000 / perfState.fps
	perfState.lastHeartbeat = os.clock()
	perfState.sampleCount += 1
end)

local function onMessage(channelId: number, client: WebStreamClient, message: string)
	local okDecode, payload = pcall(function()
		return HttpService:JSONDecode(message)
	end)
	if not okDecode or type(payload) ~= "table" then
		sendEnvelope(client, {
			id = nil,
			ok = false,
			error = "Invalid JSON payload",
			channel = channelId,
		})
		return
	end

	local id = payload.id
	local method = payload.method
	if type(method) ~= "string" then
		sendEnvelope(client, {
			id = id,
			ok = false,
			error = "Missing method",
			channel = channelId,
		})
		return
	end

	local okCall, result = pcall(handleMethod, method, payload.params)
	if okCall then
		sendEnvelope(client, {
			id = id,
			ok = true,
			result = result,
			channel = channelId,
		})
	else
		sendEnvelope(client, {
			id = id,
			ok = false,
			error = tostring(result),
			channel = channelId,
		})
	end
end

local connectChannel

local function scheduleReconnect(channel)
	if not channel.shouldReconnect then
		return
	end
	if channel.reconnectScheduled then
		return
	end
	channel.reconnectScheduled = true
	task.delay(RECONNECT_SECONDS, function()
		if not channel.shouldReconnect then
			channel.reconnectScheduled = false
			return
		end
		channel.reconnectScheduled = false
		if channel.client ~= nil then
			return
		end
		connectChannel(channel)
	end)
end

local function closeChannel(channel)
	channel.connecting = false
	channel.open = false
	channel.reconnectScheduled = false
	local client = channel.client
	channel.client = nil
	if client then
		pcall(function()
			client:Close()
		end)
	end
	UI.updateStatusText()
end

connectChannel = function(channel)
	if not channel.shouldReconnect then
		return
	end
	if channel.client ~= nil or channel.connecting then
		return
	end

	channel.connecting = true
	channel.open = false
	UI.updateStatusText()

	local url = string.format("ws://%s:%d", host, channel.port)
	local ok, client = pcall(function()
		return HttpService:CreateWebStreamClient(Enum.WebStreamClientType.WebSocket, {
			Url = url,
		})
	end)

	if not ok or not client then
		channel.connecting = false
		channel.open = false
		UI.updateStatusText()
		scheduleReconnect(channel)
		return
	end

	channel.client = client
	channel.connecting = false
	channel.open = false

	client.Opened:Connect(function(_statusCode, _headers)
		if channel.client ~= client then
			return
		end
		channel.connecting = false
		channel.open = true
		channel.reconnectScheduled = false
		UI.updateStatusText()
		sendEnvelope(client, {
			id = nil,
			ok = true,
			event = "hello",
			channel = channel.id,
			version = "1.0.0",
		})
	end)

	client.MessageReceived:Connect(function(message: string)
		if channel.client ~= client then
			return
		end
		onMessage(channel.id, client, message)
	end)

	client.Error:Connect(function(_statusCode, _errorMessage)
		if channel.client ~= client then
			return
		end
		channel.connecting = false
		channel.open = false
		UI.updateStatusText()
		pcall(function()
			client:Close()
		end)
	end)

	client.Closed:Connect(function()
		if channel.client ~= client then
			return
		end
		channel.client = nil
		channel.connecting = false
		channel.open = false
		UI.updateStatusText()
		scheduleReconnect(channel)
	end)

	UI.updateStatusText()
end

local function reconnectAll()
	if not BRIDGE_ENABLED then
		UI.updateStatusText()
		return
	end
	for _, channel in ipairs(channels) do
		channel.shouldReconnect = true
		closeChannel(channel)
		connectChannel(channel)
	end
end

local function resetChannels()
	for _, channel in ipairs(channels) do
		channel.shouldReconnect = false
		closeChannel(channel)
	end
	table.clear(channels)
	for i, port in ipairs(ports) do
		channels[i] = {
			id = i,
			port = port,
			client = nil,
			open = false,
			connecting = false,
			reconnectScheduled = false,
			shouldReconnect = BRIDGE_ENABLED,
		}
	end
end

local function setBridgeEnabled(enabled: boolean)
	BRIDGE_ENABLED = enabled == true
	SettingsModule.saveEnabled(plugin, SETTINGS_PREFIX, BRIDGE_ENABLED)
	for _, channel in ipairs(channels) do
		channel.shouldReconnect = BRIDGE_ENABLED
		if not BRIDGE_ENABLED then
			closeChannel(channel)
		end
	end
	if BRIDGE_ENABLED then
		reconnectAll()
	end
	UI.updateEnabledState()
	UI.updateStatusText()
end

function Config.parsePortsCsv(raw: string): { number }?
	return SettingsModule.parsePortsCsv(raw)
end

function Config.applyWidgetSettings()
	local nextHost = string.gsub(hostBox.Text or "", "^%s*(.-)%s*$", "%1")
	if nextHost == "" then
		nextHost = DEFAULT_HOST
	end

	local parsedPorts = Config.parsePortsCsv(portsBox.Text or "")
	if parsedPorts == nil then
		warn("[ParallelExportBridge] invalid ports list, expected comma-separated numbers")
		portsBox.Text = table.concat(ports, ",")
		return
	end

	host = nextHost
	ports = parsedPorts
	SettingsModule.saveHostPorts(plugin, SETTINGS_PREFIX, host, ports)

	resetChannels()
	UI.updateStatusText()
	if BRIDGE_ENABLED then
		reconnectAll()
	end
end

preSerializeButton.MouseButton1Click:Connect(function()
	configureExportOptions({
		preSerializeOnPrepare = not PRE_SERIALIZE_ON_PREPARE,
	})
end)

applyButton.MouseButton1Click:Connect(function()
	Config.applyWidgetSettings()
end)

enabledButton.MouseButton1Click:Connect(function()
	setBridgeEnabled(not BRIDGE_ENABLED)
end)

openButton.Click:Connect(UI.showWidget)
toggleButton.Click:Connect(function()
	setBridgeEnabled(not BRIDGE_ENABLED)
	UI.showWidget()
end)
reconnectButton.Click:Connect(function()
	UI.showWidget()
	if not BRIDGE_ENABLED then
		setBridgeEnabled(true)
	else
		reconnectAll()
	end
end)
plugin.Unloading:Connect(function()
	for _, channel in ipairs(channels) do
		channel.shouldReconnect = false
		closeChannel(channel)
	end
end)

resetChannels()
UI.updateEnabledState()
UI.updateStatusText()
if BRIDGE_ENABLED then
	reconnectAll()
end
