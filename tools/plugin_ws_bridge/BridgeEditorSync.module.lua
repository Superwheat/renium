local BridgeEditorSync = {}

local CollectionService = game:GetService("CollectionService")
local InsertService = game:GetService("InsertService")
local Workspace = game:GetService("Workspace")
local ScriptEditorService = nil
pcall(function()
	ScriptEditorService = game:GetService("ScriptEditorService")
end)

local RbxDomModule = nil
do
	local parent = script.Parent
	local rbxDom = parent and parent:FindFirstChild("RbxDom")
	if rbxDom and rbxDom:IsA("ModuleScript") then
		local ok, result = pcall(require, rbxDom)
		if ok and type(result) == "table" then
			RbxDomModule = result
		end
	end
end

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

local PATH_SEPARATOR = "\0"
local reconcileSessions = {}
local recentlyCreatedMeshPartKeys = {}

local function pathKey(pathSegments: any): string
	if type(pathSegments) ~= "table" then
		return ""
	end
	local out = table.create(#pathSegments)
	for i = 1, #pathSegments do
		out[i] = tostring(pathSegments[i])
	end
	return table.concat(out, PATH_SEPARATOR)
end

local function pathOrdinalsKey(pathOrdinals: any): string
	if type(pathOrdinals) ~= "table" then
		return ""
	end
	local out = table.create(#pathOrdinals)
	for i = 1, #pathOrdinals do
		out[i] = tostring(tonumber(pathOrdinals[i]) or 1)
	end
	return table.concat(out, ",")
end

local function pathCacheKey(pathSegments: any, pathOrdinals: any): string
	local base = pathKey(pathSegments)
	local ordinals = pathOrdinalsKey(pathOrdinals)
	if ordinals == "" then
		return base
	end
	return base .. PATH_SEPARATOR .. "ord" .. PATH_SEPARATOR .. ordinals
end

local function resolveOrdinalChild(parent: Instance, childName: string, ordinal: number): Instance?
	if ordinal <= 1 then
		return parent:FindFirstChild(childName)
	end
	local seen = 0
	for _, child in ipairs(parent:GetChildren()) do
		if child.Name == childName then
			seen += 1
			if seen == ordinal then
				return child
			end
		end
	end
	return nil
end

local function resolvePathSegments(pathSegments: any, resolveCache: { [string]: any }?, pathOrdinals: any?): Instance?
	if type(pathSegments) ~= "table" or #pathSegments == 0 then
		return nil
	end

	local cacheKey = nil
	if resolveCache ~= nil then
		cacheKey = pathCacheKey(pathSegments, pathOrdinals)
		local cached = resolveCache[cacheKey]
		if cached ~= nil then
			if cached == false then
				return nil
			end
			if typeof(cached) == "Instance" and cached.Parent ~= nil and cached:IsDescendantOf(game) then
				return cached
			end
		end
	end

	local first = tostring(pathSegments[1])
	local okService, current = pcall(function()
		return game:GetService(first)
	end)
	if not okService or current == nil then
		current = game:FindFirstChild(first)
	end
	if current == nil then
		if resolveCache ~= nil and cacheKey ~= nil then
			resolveCache[cacheKey] = false
		end
		return nil
	end

	for i = 2, #pathSegments do
		local ordinal = 1
		if type(pathOrdinals) == "table" then
			ordinal = tonumber(pathOrdinals[i]) or 1
		end
		current = resolveOrdinalChild(current, tostring(pathSegments[i]), ordinal)
		if current == nil then
			if resolveCache ~= nil and cacheKey ~= nil then
				resolveCache[cacheKey] = false
			end
			return nil
		end
	end
	if resolveCache ~= nil and cacheKey ~= nil then
		resolveCache[cacheKey] = current
	end
	return current
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

local function liveInstance(value: any): Instance?
	if typeof(value) == "Instance" and value.Parent ~= nil and value:IsDescendantOf(game) then
		return value
	end
	return nil
end

local function getStateForService(serviceName: string, ctx: { [string]: any }): any
	if serviceName == "" or type(ctx.getState) ~= "function" then
		return nil
	end
	local okState, state = pcall(ctx.getState, serviceName)
	if okState and state ~= nil then
		return state
	end
	return nil
end

local function parseInstanceIndexId(settingsId: string, identityModule: any): number?
	if type(identityModule) == "table" and type(identityModule.parseInstanceIndexId) == "function" then
		local okIndex, index = pcall(identityModule.parseInstanceIndexId, settingsId)
		if okIndex and type(index) == "number" then
			return index
		end
	end
	return tonumber(settingsId, 16) or tonumber(settingsId)
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

	local state = getStateForService(serviceName, ctx)
	if state == nil or type(state.instances) ~= "table" then
		ctx.settingsIdLookupByService[serviceName] = {
			lookup = {},
			state = state,
		}
		return ctx.settingsIdLookupByService[serviceName].lookup, state
	end

	local lookup = {}
	local identityModule = ctx.identityModule
	for index, candidate in ipairs(state.instances) do
		local instance = liveInstance(candidate)
		if instance ~= nil then
			lookup[tostring(index)] = instance
			lookup[string.format("%x", index)] = instance
			if type(state.instanceIdByInstance) == "table" then
				local instanceId = state.instanceIdByInstance[instance]
				if type(instanceId) == "number" and instanceId >= 1 then
					lookup[tostring(instanceId)] = instance
					lookup[string.format("%x", instanceId)] = instance
				elseif type(instanceId) == "string" and instanceId ~= "" then
					lookup[instanceId] = instance
				end
			end
			if type(identityModule) == "table" and type(identityModule.getCachedDebugId) == "function" then
				local okDebug, debugId = pcall(identityModule.getCachedDebugId, state, instance)
				if okDebug and type(debugId) == "string" and debugId ~= "" then
					lookup["debug:" .. debugId] = instance
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
	if index ~= nil and index >= 1 then
		instance = liveInstance(lookup[tostring(index)] or lookup[string.format("%x", index)])
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

local function resolveInstance(change: { [string]: any }, ctx: { [string]: any }): Instance?
	local serviceName = tostring(change.service or "")
	local instance = resolveInstanceBySettingsId(serviceName, change.settingsId, ctx)
	-- A bare-hex settings id is an instance index pinned by an earlier export; if
	-- the Studio tree shifted since, the same id can resolve to a different
	-- instance. Reject class mismatches so the apply falls back to the explicit
	-- path instead of silently writing to the wrong instance.
	if instance ~= nil and not instanceMatchesExpectedClass(instance, change.className) then
		instance = nil
	end
	if instance ~= nil then
		return instance
	end

	local pathSegments = change.pathSegments
	if type(pathSegments) == "table" and #pathSegments > 0 then
		return resolvePathSegments(pathSegments, ctx.resolveCache, change.pathOrdinals)
	end
	return nil
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

local function resolveEntryInstance(entry: { [string]: any }, serviceName: string, ctx: { [string]: any }): Instance?
	local instance = resolveInstanceBySettingsId(serviceName, entry.settingsId, ctx)
	if instance ~= nil then
		return instance
	end
	return resolvePathSegments(entry.pathSegments, nil, entry.pathOrdinals)
end

local function syncEntryPlacement(entry: { [string]: any }, instance: Instance, stats: { [string]: any }, resolvedEntries: { [string]: any }?)
	local parent = resolveEntryParent(entry, resolvedEntries)
	if parent == nil then
		error("Cannot place instance; parent path was not found: " .. tostring(entry.key))
	end
	if instance.Parent ~= parent then
		instance.Parent = parent
		stats.propertyUpdated += 1
	end
	local nextName = tostring(entry.pathSegments[#entry.pathSegments] or instance.Name)
	if nextName ~= "" and instance.Name ~= nextName then
		instance.Name = nextName
		stats.propertyUpdated += 1
	end
end

local function instanceSettingsIdKeys(serviceName: string, instance: Instance, ctx: { [string]: any }): { [string]: boolean }
	local keys = {}
	local state = getStateForService(serviceName, ctx)
	if state == nil then
		return keys
	end
	if type(state.instanceIdByInstance) == "table" then
		local instanceId = state.instanceIdByInstance[instance]
		if type(instanceId) == "number" and instanceId >= 1 then
			keys[tostring(instanceId)] = true
			keys[string.format("%x", instanceId)] = true
		elseif type(instanceId) == "string" and instanceId ~= "" then
			keys[instanceId] = true
		end
	end
	if type(state.instanceIndexByInstance) == "table" then
		local instanceIndex = state.instanceIndexByInstance[instance]
		if type(instanceIndex) == "number" and instanceIndex >= 1 then
			keys[tostring(instanceIndex)] = true
			keys[string.format("%x", instanceIndex)] = true
		end
	end
	local identityModule = ctx.identityModule
	if type(identityModule) == "table" then
		if type(identityModule.getCachedInstanceIndex) == "function" then
			local okIndex, index = pcall(identityModule.getCachedInstanceIndex, state, instance)
			if okIndex and type(index) == "number" and index >= 1 then
				keys[tostring(index)] = true
				keys[string.format("%x", index)] = true
			end
		end
		if type(identityModule.getCachedDebugId) == "function" then
			local okDebug, debugId = pcall(identityModule.getCachedDebugId, state, instance)
			if okDebug and type(debugId) == "string" and debugId ~= "" then
				keys["debug:" .. debugId] = true
			end
		end
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
	local state = getStateForService(serviceName, ctx)
	if state == nil then
		return
	end
	local keys = instanceSettingsIdKeys(serviceName, oldInstance, ctx)
	local settingsId = settingsIdText(rawSettingsId)
	if settingsId ~= nil then
		keys[settingsId] = true
	end

	local index = if settingsId ~= nil then parseInstanceIndexId(settingsId, ctx.identityModule) else nil
	if type(index) ~= "number" or index < 1 then
		index = nil
	end
	if type(state.instances) == "table" then
		local indexedInstance = if index ~= nil then liveInstance(state.instances[index]) else nil
		if index == nil or indexedInstance ~= oldInstance then
			for candidateIndex, candidate in ipairs(state.instances) do
				if candidate == oldInstance then
					index = candidateIndex
					break
				end
			end
		end
		if index ~= nil then
			state.instances[index] = replacement
			keys[tostring(index)] = true
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
		elseif index ~= nil then
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
	ctx: { [string]: any },
	desiredSettingsIds: { [string]: boolean },
	desiredStableKeys: { [string]: boolean }
)
	local settingsId = entry.settingsId
	if settingsId == nil then
		return
	end
	local instance = resolveInstanceBySettingsId(serviceName, settingsId, ctx)
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
	if next(desiredSettingsIds) == nil then
		return false
	end
	for key in pairs(instanceSettingsIdKeys(serviceName, instance, ctx)) do
		if desiredSettingsIds[key] == true then
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
	local legacyKey = pathSegments and pathKey(pathSegments) or ""
	if key ~= "" and desiredStableKeys[key] == true then
		return false
	end
	return key ~= "" and (desiredKeys[key] == true or desiredKeys[legacyKey] == true)
end

local function getInstancePathSegments(instance: Instance): { string }?
	if instance == game or not instance:IsDescendantOf(game) then
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

local function getSiblingNameOrdinal(instance: Instance): number
	local parent = instance.Parent
	if parent == nil then
		return 1
	end
	local ordinal = 0
	for _, child in ipairs(parent:GetChildren()) do
		if child.Name == instance.Name then
			ordinal += 1
			if child == instance then
				return ordinal
			end
		end
	end
	return 1
end

local function getInstancePathOrdinals(instance: Instance): { number }?
	if instance == game or not instance:IsDescendantOf(game) then
		return nil
	end
	local ordinals = {}
	local current: Instance? = instance
	while current ~= nil and current ~= game do
		table.insert(ordinals, 1, getSiblingNameOrdinal(current))
		current = current.Parent
	end
	return ordinals
end

local function getServiceRoot(serviceName: string): Instance?
	if serviceName == "" then
		return nil
	end
	local okService, service = pcall(function()
		return game:GetService(serviceName)
	end)
	if okService and service ~= nil then
		return service
	end
	return game:FindFirstChild(serviceName)
end

local function assertAllowedService(serviceName: string, ctx: { [string]: any }): Instance
	if serviceName == "" or type(ctx.allowedServices) ~= "table" or ctx.allowedServices[serviceName] ~= true then
		error("Refusing editor mutation outside an allowed service: " .. tostring(serviceName))
	end
	local service = getServiceRoot(serviceName)
	if service == nil then
		error("Service was not found: " .. serviceName)
	end
	return service
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

local function numberField(raw: { [string]: any }, name: string, fallback: number?): number
	local value = tonumber(raw[name])
	if value == nil then
		return fallback or 0
	end
	return value
end

local function decodeColor3(raw: any): (boolean, any)
	if typeof(raw) == "Color3" then
		return true, raw
	end
	if type(raw) ~= "table" then
		return false, "Color3 value must be a table"
	end
	return true, Color3.new(
		tonumber(raw.r or raw.R or raw[1]) or 0,
		tonumber(raw.g or raw.G or raw[2]) or 0,
		tonumber(raw.b or raw.B or raw[3]) or 0
	)
end

local function decodeEnumItem(raw: { [string]: any }, enumHint: string?): (boolean, any)
	local enumType = tostring(raw.enumType or "")
	if enumType == "" and enumHint ~= nil and enumHint ~= "" then
		if string.sub(enumHint, 1, 5) == "Enum." then
			enumType = enumHint
		else
			enumType = "Enum." .. enumHint
		end
	end
	local enumName = string.gsub(enumType, "^Enum%.", "")
	local itemName = tostring(raw.name or "")
	local ok, item = pcall(function()
		return (Enum :: any)[enumName][itemName]
	end)
	if ok and item ~= nil then
		return true, item
	end
	return false, ("Unknown enum item %s.%s"):format(enumType, itemName)
end

local function decodeRefValue(raw: { [string]: any }, ctx: { [string]: any }?, serviceName: string?): any
	if type(ctx) == "table" and type(serviceName) == "string" and serviceName ~= "" then
		local instance = resolveInstanceBySettingsId(serviceName, raw.settingsId or raw.instanceId or raw.instanceIndex, ctx)
		if instance ~= nil then
			return instance
		end
	end
	if type(raw.pathSegments) == "table" then
		return resolvePathSegments(raw.pathSegments)
	end
	return nil
end

local function decodeValue(raw: any, enumHint: string?, ctx: { [string]: any }?, serviceName: string?): (boolean, any)
	if type(raw) ~= "table" then
		return true, raw
	end

	local typeName = raw._type
	if typeName == nil and enumHint == "FontFace" and raw.family ~= nil then
		typeName = "Font"
	end
	if typeName == nil and raw.BrickColor ~= nil then
		typeName = "BrickColor"
	end
	if typeName == nil and type(raw.ColorSequence) == "table" then
		raw = raw.ColorSequence
		typeName = "ColorSequence"
	end
	if typeName == nil and type(raw.NumberSequence) == "table" then
		raw = raw.NumberSequence
		typeName = "NumberSequence"
	end
	if typeName == nil and type(raw.Ref) == "table" then
		raw = raw.Ref
		typeName = "Ref"
	end
	if typeName == nil then
		return true, raw
	end
	typeName = tostring(typeName)

	if typeName == "Vector2" then
		return true, Vector2.new(numberField(raw, "x"), numberField(raw, "y"))
	elseif typeName == "Vector3" then
		return true, Vector3.new(numberField(raw, "x"), numberField(raw, "y"), numberField(raw, "z"))
	elseif typeName == "UDim" then
		return true, UDim.new(numberField(raw, "scale"), numberField(raw, "offset"))
	elseif typeName == "UDim2" then
		return true, UDim2.new(
			numberField(raw, "xScale"),
			numberField(raw, "xOffset"),
			numberField(raw, "yScale"),
			numberField(raw, "yOffset")
		)
	elseif typeName == "Color3" then
		return decodeColor3(raw)
	elseif typeName == "BrickColor" then
		return true, BrickColor.new(tonumber(raw.number or raw.BrickColor) or 0)
	elseif typeName == "EnumItem" then
		return decodeEnumItem(raw, enumHint)
	elseif typeName == "CFrame" then
		local components = raw.components
		if type(components) ~= "table" or #components ~= 12 then
			return false, "CFrame components must contain 12 numbers"
		end
		local values = table.create(12)
		for i = 1, 12 do
			values[i] = tonumber(components[i]) or 0
		end
		return true, CFrame.new(table.unpack(values))
	elseif typeName == "Rect" then
		return true, Rect.new(numberField(raw, "minX"), numberField(raw, "minY"), numberField(raw, "maxX"), numberField(raw, "maxY"))
	elseif typeName == "Font" then
		local ok, font = pcall(function()
			return Font.new(
				tostring(raw.family or ""),
				(Enum.FontWeight :: any)[tostring(raw.weight or "Regular")],
				(Enum.FontStyle :: any)[tostring(raw.style or "Normal")]
			)
		end)
		if ok then
			return true, font
		end
		return false, font
	elseif typeName == "ColorSequence" then
		local keypoints = raw.keypoints
		if type(keypoints) ~= "table" then
			return false, "ColorSequence keypoints must be a table"
		end
		local decoded = table.create(#keypoints)
		for i, keypoint in ipairs(keypoints) do
			local colorRaw = keypoint.value
			if colorRaw == nil then
				colorRaw = keypoint.color or keypoint.Value
			end
			local okColor, color = decodeColor3(colorRaw)
			if not okColor then
				return false, color
			end
			decoded[i] = ColorSequenceKeypoint.new(numberField(keypoint, "time"), color)
		end
		return true, ColorSequence.new(decoded)
	elseif typeName == "NumberSequence" then
		local keypoints = raw.keypoints
		if type(keypoints) ~= "table" then
			return false, "NumberSequence keypoints must be a table"
		end
		local decoded = table.create(#keypoints)
		for i, keypoint in ipairs(keypoints) do
			decoded[i] = NumberSequenceKeypoint.new(
				numberField(keypoint, "time"),
				numberField(keypoint, "value"),
				numberField(keypoint, "envelope")
			)
		end
		return true, NumberSequence.new(decoded)
	elseif typeName == "Axes" then
		local axes = {}
		for _, name in ipairs(raw.axes or {}) do
			local item = (Enum.Axis :: any)[tostring(name)]
			if item ~= nil then
				axes[#axes + 1] = item
			end
		end
		return true, Axes.new(table.unpack(axes))
	elseif typeName == "Faces" then
		local faces = {}
		for _, name in ipairs(raw.faces or {}) do
			local item = (Enum.NormalId :: any)[tostring(name)]
			if item ~= nil then
				faces[#faces + 1] = item
			end
		end
		return true, Faces.new(table.unpack(faces))
	elseif typeName == "Ray" then
		local origin = raw.origin or {}
		local direction = raw.direction or {}
		return true, Ray.new(
			Vector3.new(numberField(origin, "x"), numberField(origin, "y"), numberField(origin, "z")),
			Vector3.new(numberField(direction, "x"), numberField(direction, "y"), numberField(direction, "z"))
		)
	elseif typeName == "Ref" then
		return true, decodeRefValue(raw, ctx, serviceName)
	end

	return true, raw
end

local function enumHintForProperty(instance: Instance, propertyName: string): string?
	if RbxDomModule ~= nil and RbxDomModule.findCanonicalPropertyDescriptor ~= nil then
		local ok, descriptor = pcall(RbxDomModule.findCanonicalPropertyDescriptor, instance.ClassName, propertyName)
		if ok and descriptor ~= nil and type(descriptor.enumType) == "string" and descriptor.enumType ~= "" then
			return descriptor.enumType
		end
	end
	-- The current value names the real enum type even when the property is
	-- missing from the reflection database; the property name alone is wrong
	-- whenever they differ (e.g. Shape -> Enum.PartType).
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
		if propertyName == "Scale" or propertyName == "WorldPivot" or propertyName == "WorldPivotData" or propertyName == "Origin" then
			return true
		end
	end
	if RbxDomModule ~= nil and RbxDomModule.findCanonicalPropertyDescriptor ~= nil then
		local ok, descriptor = pcall(RbxDomModule.findCanonicalPropertyDescriptor, instance.ClassName, propertyName)
		if ok then
			return descriptor ~= nil
		end
	end
	local okRead = pcall(function()
		return (instance :: any)[propertyName]
	end)
	return okRead
end

local function decodePropertyValue(instance: Instance, propertyName: string, rawValue: any, ctx: { [string]: any }, serviceName: string): (boolean, any)
	return decodeValue(rawValue, enumHintForProperty(instance, propertyName), ctx, serviceName)
end

local function tableValuesEqual(a: { [any]: any }, b: { [any]: any }): boolean
	for key, value in pairs(a) do
		if b[key] ~= value then
			return false
		end
	end
	for key in pairs(b) do
		if a[key] == nil then
			return false
		end
	end
	return true
end

local function valuesEqual(a: any, b: any): boolean
	if a == b then
		return true
	end
	if type(a) == "table" and type(b) == "table" then
		return tableValuesEqual(a, b)
	end
	return false
end

local function connectProbeSignal(stats: { [string]: any }, eventName: string, countField: string, availableField: string, connections: { RBXScriptConnection })
	local okSignal, signal = pcall(function()
		return (game :: any)[eventName]
	end)
	if not okSignal or signal == nil then
		return
	end
	local okConnect, connection = pcall(function()
		return signal:Connect(function()
			stats[countField] += 1
		end)
	end)
	if okConnect and connection ~= nil then
		stats[availableField] = 1
		table.insert(connections, connection)
	end
end

local function startEventProbe(stats: { [string]: any }): () -> ()
	local connections = {}
	connectProbeSignal(stats, "ItemChanged", "probeItemChanged", "probeItemChangedAvailable", connections)
	connectProbeSignal(stats, "DescendantAdded", "probeDescendantAdded", "probeDescendantAddedAvailable", connections)
	connectProbeSignal(stats, "DescendantRemoving", "probeDescendantRemoving", "probeDescendantRemovingAvailable", connections)

	return function()
		for _, connection in ipairs(connections) do
			connection:Disconnect()
		end
	end
end

local function readProperty(instance: Instance, propertyName: string): (boolean, any)
	if instance:IsA("Model") or instance:IsA("WorldModel") then
		if propertyName == "Scale" then
			return pcall(function()
				return (instance :: any):GetScale()
			end)
		elseif propertyName == "WorldPivot" or propertyName == "WorldPivotData" or propertyName == "Origin" then
			return pcall(function()
				return (instance :: any):GetPivot()
			end)
		end
	end
	if RbxDomModule ~= nil then
		local okCall, okRead, value = pcall(RbxDomModule.readProperty, instance, propertyName)
		if okCall and okRead then
			return true, value
		end
	end
	return pcall(function()
		return (instance :: any)[propertyName]
	end)
end

local function writeProperty(instance: Instance, propertyName: string, value: any): (boolean, any)
	if instance:IsA("Model") or instance:IsA("WorldModel") then
		if propertyName == "Scale" then
			return pcall(function()
				return (instance :: any):ScaleTo(value)
			end)
		elseif propertyName == "Origin" then
			return pcall(function()
				return (instance :: any):PivotTo(value)
			end)
		elseif propertyName == "WorldPivot" or propertyName == "WorldPivotData" then
			return pcall(function()
				(instance :: any).WorldPivot = value
			end)
		end
	end
	if RbxDomModule ~= nil then
		local okCall, okWrite = pcall(RbxDomModule.writeProperty, instance, propertyName, value)
		if okCall and okWrite then
			return true, nil
		end
	end
	local okWrite, writeErr = pcall(function()
		(instance :: any)[propertyName] = value
	end)
	if okWrite then
		return true, nil
	end
	-- Content-typed properties (e.g. Decal.TextureContent) reject a plain asset
	-- string; wrap it in a Content value and retry before reporting failure.
	if type(value) == "string" and Content ~= nil and (Content :: any).fromUri ~= nil then
		local okContent = pcall(function()
			(instance :: any)[propertyName] = if value == ""
				then (Content :: any).none
				else (Content :: any).fromUri(value)
		end)
		if okContent then
			return true, nil
		end
	end
	return false, writeErr
end

local function applyMeshPartMeshId(instance: Instance, meshId: any): (boolean, any)
	if not instance:IsA("MeshPart") then
		return false, "MeshId fallback only supports MeshPart"
	end

	local meshIdText = tostring(meshId or "")
	if meshIdText == "" then
		return false, "Cannot apply empty MeshId through CreateMeshPartAsync"
	end

	local collisionFidelity = Enum.CollisionFidelity.Default
	pcall(function()
		collisionFidelity = (instance :: any).CollisionFidelity
	end)
	local renderFidelity = Enum.RenderFidelity.Automatic
	pcall(function()
		renderFidelity = (instance :: any).RenderFidelity
	end)

	local okCreate, meshPartOrErr = pcall(function()
		return InsertService:CreateMeshPartAsync(meshIdText, collisionFidelity, renderFidelity)
	end)
	if not okCreate or meshPartOrErr == nil then
		return false, meshPartOrErr
	end

	local sourceMeshPart = meshPartOrErr :: MeshPart
	local okApply, applyErr = pcall(function()
		(instance :: MeshPart):ApplyMesh(sourceMeshPart)
	end)
	pcall(function()
		sourceMeshPart:Destroy()
	end)
	if not okApply then
		return false, applyErr
	end
	return true, nil
end

local function canApplyProtectedMeshId(change: { [string]: any }, instance: Instance): boolean
	if change.allowProtectedMeshIdApply == true then
		return true
	end
	if not instance:IsA("MeshPart") then
		return false
	end
	return recentlyCreatedMeshPartKeys[pathCacheKey(change.pathSegments, change.pathOrdinals)] == true
		or recentlyCreatedMeshPartKeys[pathKey(change.pathSegments)] == true
end

local function clearRecentlyCreatedMeshPart(change: { [string]: any })
	recentlyCreatedMeshPartKeys[pathCacheKey(change.pathSegments, change.pathOrdinals)] = nil
	recentlyCreatedMeshPartKeys[pathKey(change.pathSegments)] = nil
end

local function applyTags(instance: Instance, rawTags: any, stats: { [string]: any })
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
		if desired[tag] ~= true then
			CollectionService:RemoveTag(instance, tag)
			changed = true
		end
		desired[tag] = nil
	end
	for tag in pairs(desired) do
		CollectionService:AddTag(instance, tag)
		changed = true
	end
	if changed then
		stats.propertyUpdated += 1
	else
		stats.noops += 1
	end
end

local function replaceInstanceClass(instance: Instance, className: string, stats: { [string]: any }): Instance
	if instance.ClassName == className then
		stats.noops += 1
		return instance
	end

	local parent = instance.Parent
	if parent == nil then
		error("Cannot replace service root class for " .. instance:GetFullName())
	end
	local okCreate, replacement = pcall(Instance.new, className)
	if not okCreate or replacement == nil then
		error("Cannot create replacement " .. className .. " for " .. instance:GetFullName() .. ": " .. tostring(replacement))
	end
	replacement.Name = instance.Name
	-- Properties are re-sent by the editor push after a class replace, but
	-- attributes and tags are not always part of that batch; carry them over.
	local okAttributes, attributes = pcall(function()
		return instance:GetAttributes()
	end)
	if okAttributes and type(attributes) == "table" then
		for attributeName, attributeValue in pairs(attributes) do
			pcall(function()
				replacement:SetAttribute(attributeName, attributeValue)
			end)
		end
	end
	for _, tag in ipairs(CollectionService:GetTags(instance)) do
		pcall(function()
			CollectionService:AddTag(replacement, tag)
		end)
	end
	replacement.Parent = parent
	for _, child in ipairs(instance:GetChildren()) do
		child.Parent = replacement
	end
	instance:Destroy()
	stats.instanceReplaced += 1
	return replacement
end

local function findScriptDocument(instance: Instance): any?
	if ScriptEditorService == nil then
		return nil
	end
	local ok, document = pcall(function()
		return (ScriptEditorService :: any):FindScriptDocument(instance)
	end)
	if ok and document ~= nil then
		return document
	end
	return nil
end

local function readScriptSource(instance: Instance): (boolean, any)
	if ScriptEditorService ~= nil then
		local okEditor, editorSource = pcall(function()
			return (ScriptEditorService :: any):GetEditorSource(instance)
		end)
		if okEditor then
			return true, editorSource
		end
	end
	return pcall(function()
		return (instance :: any).Source
	end)
end

local function documentLineEndCharacter(document: any, line: number): number
	local okLine, lineText = pcall(function()
		return document:GetLine(line)
	end)
	if okLine and type(lineText) == "string" then
		return #lineText + 1
	end
	return 1
end

local function clampDocumentPosition(document: any, line: any, character: any): (number, number)
	local okCount, lineCount = pcall(function()
		return document:GetLineCount()
	end)
	if not okCount or type(lineCount) ~= "number" or lineCount < 1 then
		return 1, 1
	end
	local clampedLine = math.clamp(math.floor(tonumber(line) or 1), 1, lineCount)
	local lineEnd = documentLineEndCharacter(document, clampedLine)
	local clampedCharacter = math.clamp(math.floor(tonumber(character) or 1), 1, lineEnd)
	return clampedLine, clampedCharacter
end

local function getDocumentSelection(document: any): { number }?
	local okSelection, cursorLine, cursorCharacter, anchorLine, anchorCharacter = pcall(function()
		return document:GetSelection()
	end)
	if not okSelection or type(cursorLine) ~= "number" or type(cursorCharacter) ~= "number" then
		return nil
	end
	if type(anchorLine) ~= "number" then
		anchorLine = cursorLine
	end
	if type(anchorCharacter) ~= "number" then
		anchorCharacter = cursorCharacter
	end
	return { cursorLine, cursorCharacter, anchorLine, anchorCharacter }
end

local function restoreDocumentSelection(document: any, selection: { number }?)
	if selection == nil then
		return
	end
	local cursorLine, cursorCharacter = clampDocumentPosition(document, selection[1], selection[2])
	local anchorLine, anchorCharacter = clampDocumentPosition(document, selection[3], selection[4])
	local okRequest, success = pcall(function()
		return document:RequestSetSelectionAsync(cursorLine, cursorCharacter, anchorLine, anchorCharacter)
	end)
	if okRequest and success ~= false then
		return
	end
	pcall(function()
		document:ForceSetSelectionAsync(cursorLine, cursorCharacter, anchorLine, anchorCharacter)
	end)
end

local function setOpenDocumentSource(document: any, source: string): (boolean, any)
	local okText, currentText = pcall(function()
		return document:GetText()
	end)
	if okText and currentText == source then
		return true, nil
	end

	local okLineCount, lineCount = pcall(function()
		return document:GetLineCount()
	end)
	if not okLineCount or type(lineCount) ~= "number" or lineCount < 1 then
		return false, "open script document did not report a valid line count"
	end

	local selection = getDocumentSelection(document)
	local endCharacter = documentLineEndCharacter(document, lineCount)
	local okEdit, success, editErr = pcall(function()
		return document:EditTextAsync(source, 1, 1, lineCount, endCharacter)
	end)
	if not okEdit then
		return false, success
	end
	if success == false then
		return false, editErr or "EditTextAsync failed"
	end
	restoreDocumentSelection(document, selection)
	return true, nil
end

local function setSource(instance: Instance, source: string): (boolean, any, string)
	local document = findScriptDocument(instance)
	if document ~= nil then
		local documentOk, documentErr = setOpenDocumentSource(document, source)
		if documentOk then
			return true, nil, "ScriptDocument"
		end
	end

	if ScriptEditorService ~= nil then
		local updateOk, updateErr = pcall(function()
			(ScriptEditorService :: any):UpdateSourceAsync(instance, function()
				return source
			end)
		end)
		if updateOk then
			return true, nil, "UpdateSourceAsync"
		end
	end
	local ok, err = pcall(function()
		(instance :: any).Source = source
	end)
	if ok then
		return true, nil, "Source"
	end
	return false, err, "Source"
end

local function syncOptions(ctx: { [string]: any }): { [string]: any }
	if type(ctx.syncOptions) == "table" then
		return ctx.syncOptions
	end
	if type(ctx.getSyncOptions) == "function" then
		local ok, options = pcall(ctx.getSyncOptions)
		if ok and type(options) == "table" then
			return options
		end
	end
	return {}
end

local function liveHydrateEnabled(ctx: { [string]: any }): boolean
	return syncOptions(ctx).liveHydrate ~= false
end

local function keepUnknownsEnabled(ctx: { [string]: any }): boolean
	return syncOptions(ctx).keepUnknowns == true
end

local function applySourceChange(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true
	if type(change.source) == "string" and #change.source > (tonumber(ctx.maxSourceBytes) or 8 * 1024 * 1024) then
		error("Editor source mutation exceeds safe size limit")
	end

	local instance = resolveInstance(change, ctx)
	if instance ~= nil then
		assertInstanceInService(instance, service)
	end
	if change.deleted == true then
		if instance ~= nil then
			if ctx.luaSourceClass[instance.ClassName] ~= true then
				error("Target is not a Lua source container: " .. instance:GetFullName())
			end
			local okWrite, err, writeMethod = setSource(instance, "")
			if not okWrite then
				error("Failed to clear Source for " .. instance:GetFullName() .. ": " .. tostring(err))
			end
			if writeMethod == "UpdateSourceAsync" then
				stats.sourceUpdateAsync += 1
			else
				stats.sourceDirect += 1
			end
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
		local parent = resolveParent(change, ctx.resolveCache)
		if parent == nil then
			error("Cannot create source instance; parent path was not found")
		end
		assertInstanceInService(parent, service)
		local okCreate, created = pcall(Instance.new, tostring(change.className or "ModuleScript"))
		if not okCreate or created == nil then
			error("Cannot create source instance: " .. tostring(created))
		end
		local pathSegments = cloneArray(change.pathSegments)
		created.Name = tostring(pathSegments[#pathSegments] or created.ClassName)
		created.Parent = parent
		if type(ctx.resolveCache) == "table" then
			ctx.resolveCache[pathCacheKey(change.pathSegments, change.pathOrdinals)] = created
		end
		instance = created
		stats.sourceCreated += 1
	end

	if ctx.luaSourceClass[instance.ClassName] ~= true then
		error("Target is not a Lua source container: " .. instance:GetFullName())
	end

	local nextSource = tostring(change.source or "")
	local okRead, currentSource = readScriptSource(instance)
	if okRead and currentSource == nextSource then
		stats.noops += 1
		return
	end

	local okWrite, err, writeMethod = setSource(instance, nextSource)
	if not okWrite then
		error("Failed to write Source for " .. instance:GetFullName() .. ": " .. tostring(err))
	end
	if writeMethod == "UpdateSourceAsync" then
		stats.sourceUpdateAsync += 1
	else
		stats.sourceDirect += 1
	end
	stats.sourceUpdated += 1
end

local function applyInstanceReconcile(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true
	if tostring(change.mode or "reconcileService") ~= "reconcileService" then
		error("Unsupported instance sync mode: " .. tostring(change.mode))
	end

	local rawInstances = change.instances
	if type(rawInstances) ~= "table" then
		return
	end
	if #rawInstances > (tonumber(ctx.maxInstanceEntriesPerChange) or 5000) then
		error("Editor instance reconcile has too many entries")
	end

	local beforeCreated = stats.instanceCreated
	local beforeDeleted = stats.instanceDeleted
	local beforeReplaced = stats.instanceReplaced
	local desiredKeys = {}
	local desiredSettingsIds = {}
	local desiredStableKeys = {}
	local desiredEntries = {}
	for _, raw in ipairs(rawInstances) do
		if type(raw) == "table" then
			local pathSegments = cloneArray(raw.pathSegments)
			if #pathSegments > 0 then
				if tostring(pathSegments[1]) ~= service.Name then
					error("Instance path root does not match service: " .. pathKey(pathSegments))
				end
				local entry = {
					pathSegments = pathSegments,
					pathOrdinals = cloneArray(raw.pathOrdinals),
					key = pathCacheKey(pathSegments, raw.pathOrdinals),
					className = tostring(raw.className or "Folder"),
					settingsId = settingsIdText(raw.settingsId),
				}
				desiredKeys[entry.key] = true
				recordDesiredStableEntry(entry, service.Name, ctx, desiredSettingsIds, desiredStableKeys)
				table.insert(desiredEntries, entry)
			end
		end
	end

	table.sort(desiredEntries, function(a, b)
		if #a.pathSegments == #b.pathSegments then
			return a.key < b.key
		end
		return #a.pathSegments < #b.pathSegments
	end)

	local resolvedEntries = {}
	for _, entry in ipairs(desiredEntries) do
		if #entry.pathSegments > 1 then
			local instance = resolveEntryInstance(entry, service.Name, ctx)
			if isProtectedWorkspaceCameraPath(entry.pathSegments) then
				stats.noops += 1
			elseif instance == nil and not liveHydrateEnabled(ctx) then
				stats.noops += 1
			elseif instance == nil then
				local parent = resolveEntryParent(entry, resolvedEntries)
				if parent == nil then
					error("Cannot create instance; parent path was not found: " .. entry.key)
				end
				local okCreate, created = pcall(Instance.new, entry.className)
				if not okCreate or created == nil then
					error("Cannot create instance " .. entry.key .. ": " .. tostring(created))
				end
				created.Name = tostring(entry.pathSegments[#entry.pathSegments])
				created.Parent = parent
				if created:IsA("MeshPart") then
					recentlyCreatedMeshPartKeys[entry.key] = true
				end
				resolvedEntries[entry.key] = created
				stats.instanceCreated += 1
			else
				syncEntryPlacement(entry, instance, stats, resolvedEntries)
				if instance.ClassName ~= entry.className then
					local oldInstance = instance
					instance = replaceInstanceClass(instance, entry.className, stats)
					rememberReplacementIdentity(service.Name, entry.settingsId, oldInstance, instance, ctx)
				end
				resolvedEntries[entry.key] = instance
			end
		end
	end

	if change.allowDeletes == true and not keepUnknownsEnabled(ctx) then
		local descendants = service:GetDescendants()
		for i = #descendants, 1, -1 do
			local instance = descendants[i]
			local pathSegments = getInstancePathSegments(instance)
			local pathOrdinals = getInstancePathOrdinals(instance)
			local key = pathSegments and pathCacheKey(pathSegments, pathOrdinals) or ""
			if key ~= "" and not shouldKeepInstanceByDesiredEntry(service.Name, instance, pathSegments, pathOrdinals, ctx, desiredKeys, desiredSettingsIds, desiredStableKeys) and not isProtectedWorkspaceCameraInstance(instance) then
				instance:Destroy()
				stats.instanceDeleted += 1
			end
		end
	end

	if stats.instanceCreated == beforeCreated and stats.instanceDeleted == beforeDeleted and stats.instanceReplaced == beforeReplaced then
		stats.noops += 1
	end
end

local function sortedInstanceEntries(change: { [string]: any }, serviceName: string)
	local rawInstances = change.instances
	local entries = {}
	if type(rawInstances) ~= "table" then
		return entries
	end
	if #rawInstances > 5000 then
		error("Editor instance mutation has too many entries")
	end

	for _, raw in ipairs(rawInstances) do
		if type(raw) == "table" then
			local pathSegments = cloneArray(raw.pathSegments)
			if #pathSegments > 0 then
				if tostring(pathSegments[1]) ~= serviceName then
					error("Instance path root does not match service: " .. pathKey(pathSegments))
				end
				table.insert(entries, {
					pathSegments = pathSegments,
					pathOrdinals = cloneArray(raw.pathOrdinals),
					key = pathCacheKey(pathSegments, raw.pathOrdinals),
					className = tostring(raw.className or "Folder"),
					settingsId = settingsIdText(raw.settingsId),
				})
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

local function reconcileSessionKey(serviceName: string, sessionId: any): string
	return serviceName .. PATH_SEPARATOR .. tostring(sessionId or "default")
end

local function applyInstanceReconcileChunk(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true

	local mode = tostring(change.mode or "")
	local sessionKey = reconcileSessionKey(serviceName, change.reconcileSession)
	if mode == "beginReconcileService" or reconcileSessions[sessionKey] == nil then
		reconcileSessions[sessionKey] = {
			desiredKeys = {},
			desiredSettingsIds = {},
			desiredStableKeys = {},
			resolvedEntries = {},
			failed = false,
		}
	end
	local session = reconcileSessions[sessionKey]
	if session.failed == true then
		if mode == "finishReconcileService" then
			reconcileSessions[sessionKey] = nil
		end
		error("Skipping reconcile chunk after an earlier chunk failed")
	end

	local beforeCreated = stats.instanceCreated
	local beforeDeleted = stats.instanceDeleted
	local beforeReplaced = stats.instanceReplaced
	for _, entry in ipairs(sortedInstanceEntries(change, service.Name)) do
		session.desiredKeys[entry.key] = true
		recordDesiredStableEntry(entry, service.Name, ctx, session.desiredSettingsIds, session.desiredStableKeys)
		if #entry.pathSegments > 1 then
			local instance = resolveEntryInstance(entry, service.Name, ctx)
			if isProtectedWorkspaceCameraPath(entry.pathSegments) then
				stats.noops += 1
			elseif instance == nil and not liveHydrateEnabled(ctx) then
				stats.noops += 1
			elseif instance == nil then
				local parent = resolveEntryParent(entry, session.resolvedEntries)
				if parent == nil then
					error("Cannot create instance; parent path was not found: " .. entry.key)
				end
				local okCreate, created = pcall(Instance.new, entry.className)
				if not okCreate or created == nil then
					error("Cannot create instance " .. entry.key .. ": " .. tostring(created))
				end
				created.Name = tostring(entry.pathSegments[#entry.pathSegments])
				created.Parent = parent
				if created:IsA("MeshPart") then
					recentlyCreatedMeshPartKeys[entry.key] = true
				end
				session.resolvedEntries[entry.key] = created
				stats.instanceCreated += 1
			else
				syncEntryPlacement(entry, instance, stats, session.resolvedEntries)
				if instance.ClassName ~= entry.className then
					local oldInstance = instance
					instance = replaceInstanceClass(instance, entry.className, stats)
					rememberReplacementIdentity(service.Name, entry.settingsId, oldInstance, instance, ctx)
				end
				session.resolvedEntries[entry.key] = instance
			end
		end
	end

	if mode == "finishReconcileService" then
		if not keepUnknownsEnabled(ctx) then
			local descendants = service:GetDescendants()
			for i = #descendants, 1, -1 do
				local instance = descendants[i]
				local pathSegments = getInstancePathSegments(instance)
				local pathOrdinals = getInstancePathOrdinals(instance)
				local key = pathSegments and pathCacheKey(pathSegments, pathOrdinals) or ""
				if key ~= "" and not shouldKeepInstanceByDesiredEntry(service.Name, instance, pathSegments, pathOrdinals, ctx, session.desiredKeys, session.desiredSettingsIds, session.desiredStableKeys) and not isProtectedWorkspaceCameraInstance(instance) then
					instance:Destroy()
					stats.instanceDeleted += 1
				end
			end
		end
		reconcileSessions[sessionKey] = nil
	end

	if stats.instanceCreated == beforeCreated and stats.instanceDeleted == beforeDeleted and stats.instanceReplaced == beforeReplaced then
		stats.noops += 1
	end
end

local function applyInstanceUpserts(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true

	local beforeCreated = stats.instanceCreated
	local beforeReplaced = stats.instanceReplaced
	local resolvedEntries = {}
	for _, entry in ipairs(sortedInstanceEntries(change, service.Name)) do
		if #entry.pathSegments > 1 then
			local instance = resolveEntryInstance(entry, service.Name, ctx)
			if isProtectedWorkspaceCameraPath(entry.pathSegments) then
				stats.noops += 1
			elseif instance == nil and not liveHydrateEnabled(ctx) then
				stats.noops += 1
			elseif instance == nil then
				local parent = resolveEntryParent(entry, resolvedEntries)
				if parent == nil then
					error("Cannot create instance; parent path was not found: " .. entry.key)
				end
				local okCreate, created = pcall(Instance.new, entry.className)
				if not okCreate or created == nil then
					error("Cannot create instance " .. entry.key .. ": " .. tostring(created))
				end
				created.Name = tostring(entry.pathSegments[#entry.pathSegments])
				created.Parent = parent
				if created:IsA("MeshPart") then
					recentlyCreatedMeshPartKeys[entry.key] = true
				end
				resolvedEntries[entry.key] = created
				stats.instanceCreated += 1
			else
				syncEntryPlacement(entry, instance, stats, resolvedEntries)
				if instance.ClassName ~= entry.className then
					local oldInstance = instance
					instance = replaceInstanceClass(instance, entry.className, stats)
					rememberReplacementIdentity(service.Name, entry.settingsId, oldInstance, instance, ctx)
				end
				resolvedEntries[entry.key] = instance
			end
		end
	end

	if stats.instanceCreated == beforeCreated and stats.instanceReplaced == beforeReplaced then
		stats.noops += 1
	end
end

local function applyInstanceDeletes(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true

	local beforeDeleted = stats.instanceDeleted
	-- Resolve every entry before destroying anything: destroying one sibling
	-- shifts the name ordinals of the rest, so resolving lazily would silently
	-- miss same-name siblings later in the batch.
	local targets = {}
	local seenTargets = {}
	for _, entry in ipairs(sortedInstanceEntries(change, service.Name)) do
		if #entry.pathSegments <= 1 then
			error("Refusing to delete service root: " .. entry.key)
		end
		local instance = resolveInstanceBySettingsId(service.Name, entry.settingsId, ctx)
		if instance ~= nil and not instanceMatchesExpectedClass(instance, entry.className) then
			-- A stale settings id resolved to a different instance; trust the path.
			instance = nil
		end
		if instance == nil then
			instance = resolvePathSegments(entry.pathSegments, nil, entry.pathOrdinals)
		end
		if instance == nil or isProtectedWorkspaceCameraInstance(instance) or seenTargets[instance] == true then
			stats.noops += 1
		else
			seenTargets[instance] = true
			targets[#targets + 1] = instance
		end
	end
	for _, instance in ipairs(targets) do
		instance:Destroy()
		stats.instanceDeleted += 1
	end

	if stats.instanceDeleted == beforeDeleted then
		stats.noops += 1
	end
end

local function applyInstanceChange(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
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

local function applyPropertyChange(change: { [string]: any }, ctx: { [string]: any }, stats: { [string]: any }, touchedServices: { [string]: boolean })
	local serviceName, service = validatedChangeService(change, ctx)
	touchedServices[serviceName] = true

	local instance = resolveInstance(change, ctx)
	if instance == nil then
		error("Target instance was not found")
	end
	assertInstanceInService(instance, service)
	if isProtectedWorkspaceCameraPath(change.pathSegments) or isProtectedWorkspaceCameraInstance(instance) then
		stats.noops += 1
		return
	end

	local properties = change.properties
	if type(properties) == "table" then
		local propertyNames = {}
		if properties.MeshId ~= nil then
			table.insert(propertyNames, "MeshId")
		end
		for propertyName in pairs(properties) do
			propertyName = tostring(propertyName)
			if propertyName ~= "MeshId" then
				table.insert(propertyNames, propertyName)
			end
		end
		for _, propertyName in ipairs(propertyNames) do
			local rawValue = properties[propertyName]
			if propertyName == "Source" then
				stats.noops += 1
			elseif propertyName == "ClassName" then
				if tostring(rawValue) == instance.ClassName then
					stats.noops += 1
				else
					error("ClassName changes are not supported for " .. instance:GetFullName())
				end
			elseif propertyName == "Name" then
				local nextName = tostring(rawValue)
				if instance.Name == nextName then
					stats.noops += 1
				else
					instance.Name = nextName
					stats.propertyUpdated += 1
				end
			elseif propertyName == "Tags" then
				applyTags(instance, rawValue, stats)
			else
				if not classHasProperty(instance, propertyName) then
					stats.noops += 1
					continue
				end
				local okDecode, decoded = decodePropertyValue(instance, propertyName, rawValue, ctx, serviceName)
				if not okDecode then
					error("Failed to decode " .. propertyName .. ": " .. tostring(decoded))
				end
				local okRead, current = readProperty(instance, propertyName)
				if okRead and valuesEqual(current, decoded) then
					stats.noops += 1
					if propertyName == "MeshId" then
						clearRecentlyCreatedMeshPart(change)
					end
				else
					if propertyName == "MeshId" and instance:IsA("MeshPart") and not canApplyProtectedMeshId(change, instance) then
						stats.noops += 1
						clearRecentlyCreatedMeshPart(change)
						continue
					end
					local okWrite, err = writeProperty(instance, propertyName, decoded)
					if not okWrite then
						if propertyName == "MeshId" and instance:IsA("MeshPart") then
							local okApplyMesh, applyMeshErr = applyMeshPartMeshId(instance, decoded)
							if not okApplyMesh then
								error("Failed to apply MeshId on " .. instance:GetFullName() .. ": " .. tostring(applyMeshErr))
							end
						else
							error("Failed to write " .. propertyName .. " on " .. instance:GetFullName() .. ": " .. tostring(err))
						end
					end
					stats.propertyUpdated += 1
					if propertyName == "MeshId" then
						clearRecentlyCreatedMeshPart(change)
					end
				end
			end
		end
	end

	local attributes = change.attributes
	if type(attributes) == "table" then
		for attributeName, rawValue in pairs(attributes) do
			attributeName = tostring(attributeName)
			local okDecode, decoded = decodeValue(rawValue, nil)
			if not okDecode then
				error("Failed to decode attribute " .. attributeName .. ": " .. tostring(decoded))
			end
			local current = instance:GetAttribute(attributeName)
			if valuesEqual(current, decoded) then
				stats.noops += 1
			else
				instance:SetAttribute(attributeName, decoded)
				stats.attributeUpdated += 1
			end
		end
	end
end

function BridgeEditorSync.create(ctx: { [string]: any })
	local api = {}
	api.stats = ctx.stats

	function api.applyChanges(params: { [string]: any }): { [string]: any }
		if type(params) ~= "table" then
			error("Editor mutation request must be an object")
		end
		local function validateChangeList(rawChanges: any, label: string)
			if rawChanges == nil then
				return
			end
			if type(rawChanges) ~= "table" then
				error("Editor " .. label .. " changes must be an array")
			end
			if #rawChanges > (tonumber(ctx.maxChangesPerRequest) or 5000) then
				error("Editor mutation request has too many " .. label .. " changes")
			end
			for _, change in ipairs(rawChanges) do
				validatedChangeService(change, ctx)
			end
		end
		-- Validate every service/path before applying the first mutation. This
		-- avoids a partially applied request that later contains an out-of-scope
		-- service or an escaping path.
		validateChangeList(params.instanceChanges, "instance")
		validateChangeList(params.sourceChanges, "source")
		validateChangeList(params.propertyChanges, "property")
		local started = os.clock()
		local previousResolveCache = ctx.resolveCache
		local previousSettingsIdLookupByService = ctx.settingsIdLookupByService
		ctx.resolveCache = {}
		ctx.settingsIdLookupByService = {}
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
			probeItemChanged = 0,
			probeDescendantAdded = 0,
			probeDescendantRemoving = 0,
			probeItemChangedAvailable = 0,
			probeDescendantAddedAvailable = 0,
			probeDescendantRemovingAvailable = 0,
		}
		local touchedServices = {}
		local stopEventProbe
		if params.probeEvents == true then
			stopEventProbe = startEventProbe(stats)
		end

		local instanceChanges = params.instanceChanges
		if type(instanceChanges) == "table" then
			for _, change in ipairs(instanceChanges) do
				local ok, err = pcall(applyInstanceChange, change, ctx, stats, touchedServices)
				if not ok then
					if type(change) == "table" then
						local mode = tostring(change.mode or "")
						if mode == "beginReconcileService" or mode == "reconcileServiceChunk" or mode == "finishReconcileService" then
							local sessionKey = reconcileSessionKey(tostring(change.service or ""), change.reconcileSession)
							if mode == "finishReconcileService" then
								reconcileSessions[sessionKey] = nil
							elseif type(reconcileSessions[sessionKey]) == "table" then
								reconcileSessions[sessionKey].failed = true
							end
						end
					end
					stats.ok = false
					stats.errors += 1
					warn("[Renium] editor instance sync failed: " .. tostring(err))
				end
			end
		end

		local sourceChanges = params.sourceChanges
		if type(sourceChanges) == "table" then
			for _, change in ipairs(sourceChanges) do
				local ok, err = pcall(applySourceChange, change, ctx, stats, touchedServices)
				if not ok then
					stats.ok = false
					stats.errors += 1
					warn("[Renium] editor source sync failed: " .. tostring(err))
				end
			end
		end

		local propertyChanges = params.propertyChanges
		if type(propertyChanges) == "table" then
			for _, change in ipairs(propertyChanges) do
				local ok, err = pcall(applyPropertyChange, change, ctx, stats, touchedServices)
				if not ok then
					stats.ok = false
					stats.errors += 1
					warn("[Renium] editor property sync failed: " .. tostring(err))
				end
			end
		end

		if stopEventProbe ~= nil then
			task.wait()
			stopEventProbe()
		end
		stats.lastMs = (os.clock() - started) * 1000
		for serviceName in pairs(touchedServices) do
			ctx.invalidateService(serviceName)
		end
		ctx.resolveCache = previousResolveCache
		ctx.settingsIdLookupByService = previousSettingsIdLookupByService
		ctx.stats.requests += 1
		ctx.stats.lastMs = stats.lastMs
		ctx.stats.lastAtUnix = os.time()
		ctx.stats.lastOk = stats.ok == true
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
		ctx.updateStatus()
		return stats
	end

	return api
end

return BridgeEditorSync
