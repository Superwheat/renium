

local HttpService = game:GetService("HttpService")


local TARGET_SERVICE = "Workspace"

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
    "Source",
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

local function tryGet(instance: Instance, propertyName: string): any
    local ok, value = pcall(function()
        return (instance :: any)[propertyName]
    end)

    if not ok then
        return nil
    end

    return value
end

local function serializeValue(value: any): any
    local valueType = typeof(value)

    if valueType == "number" or valueType == "string" or valueType == "boolean" then
        return value
    end

    if valueType == "Vector2" then
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
    elseif valueType == "ColorSequence" then
        local keypoints = {}
        for i, keypoint in ipairs(value.Keypoints) do
            keypoints[i] = {
                time = keypoint.Time,
                value = { r = keypoint.Value.R, g = keypoint.Value.G, b = keypoint.Value.B },
            }
        end
        return { _type = "ColorSequence", keypoints = keypoints }
    elseif valueType == "NumberSequence" then
        local keypoints = {}
        for i, keypoint in ipairs(value.Keypoints) do
            keypoints[i] = {
                time = keypoint.Time,
                value = keypoint.Value,
                envelope = keypoint.Envelope,
            }
        end
        return { _type = "NumberSequence", keypoints = keypoints }
    elseif valueType == "CFrame" then
        return { _type = "CFrame", components = { value:GetComponents() } }
    elseif valueType == "Rect" then
        return {
            _type = "Rect",
            minX = value.Min.X,
            minY = value.Min.Y,
            maxX = value.Max.X,
            maxY = value.Max.Y,
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
    elseif valueType == "Instance" then
        return value:GetFullName()
    end

    return nil
end

local function exportInstance(instance: Instance)
    local entry: {[string]: any} = {
        name = instance.Name,
        className = instance.ClassName,
        path = instance:GetFullName(),
        attributes = instance:GetAttributes(),
        properties = {},
    }

    local debugIdOk, debugId = pcall(function()
        return instance:GetDebugId(10)
    end)
    if debugIdOk then
        entry.debugId = debugId
    end

    for _, propertyName in ipairs(PROPERTY_CANDIDATES) do
        local value = tryGet(instance, propertyName)
        if value ~= nil then
            local serialized = serializeValue(value)
            if serialized ~= nil then
                entry.properties[propertyName] = serialized
            end
        end
    end

    return entry
end

if not ALLOWED_SERVICES[TARGET_SERVICE] then
    error("TARGET_SERVICE is not in allowed set: " .. TARGET_SERVICE)
end

local service = game:FindFirstChild(TARGET_SERVICE)
if not service then
    error("Service not found: " .. TARGET_SERVICE)
end

local instances = { exportInstance(service) }
for _, instance in ipairs(service:GetDescendants()) do
    instances[#instances + 1] = exportInstance(instance)
end

table.sort(instances, function(a, b)
    return a.path < b.path
end)

local snapshot = {
    metadata = {
        generatedAtUnix = os.time(),
        serviceName = TARGET_SERVICE,
        instanceCount = #instances,
    },
    services = {
        {
            name = service.Name,
            className = service.ClassName,
            path = service:GetFullName(),
        },
    },
    instances = instances,
}

local encoded = HttpService:JSONEncode(snapshot)
local marker = "SNAPSHOT_" .. TARGET_SERVICE
local chunkSize = 120000

print(marker .. "_BEGIN")
local i = 1
while i <= #encoded do
    print(string.sub(encoded, i, i + chunkSize - 1))
    i += chunkSize
end
print(marker .. "_END")

return {
    ok = true,
    service = TARGET_SERVICE,
    bytes = #encoded,
    chunks = math.ceil(#encoded / chunkSize),
}