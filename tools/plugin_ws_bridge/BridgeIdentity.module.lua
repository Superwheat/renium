local BridgeIdentity = {}

BridgeIdentity.PATH_SEPARATOR = "\0"

function BridgeIdentity.pathKey(pathSegments: any): string
	if type(pathSegments) ~= "table" then
		return ""
	end
	local out = table.create(#pathSegments)
	for i = 1, #pathSegments do
		out[i] = tostring(pathSegments[i])
	end
	return table.concat(out, BridgeIdentity.PATH_SEPARATOR)
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

function BridgeIdentity.pathCacheKey(pathSegments: any, pathOrdinals: any): string
	local base = BridgeIdentity.pathKey(pathSegments)
	local ordinals = pathOrdinalsKey(pathOrdinals)
	if ordinals == "" then
		return base
	end
	local separator = BridgeIdentity.PATH_SEPARATOR
	return base .. separator .. "ord" .. separator .. ordinals
end

function BridgeIdentity.resolveOrdinalChild(parent: Instance, childName: string, ordinal: number): Instance?
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

function BridgeIdentity.resolvePathSegments(
	pathSegments: any,
	resolveCache: { [string]: any }?,
	pathOrdinals: any?
): Instance?
	if type(pathSegments) ~= "table" or #pathSegments == 0 then
		return nil
	end

	local cacheKey = nil
	if resolveCache ~= nil then
		cacheKey = BridgeIdentity.pathCacheKey(pathSegments, pathOrdinals)
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

	local current = game:GetService(tostring(pathSegments[1]))
	for i = 2, #pathSegments do
		local ordinal = if type(pathOrdinals) == "table" then tonumber(pathOrdinals[i]) or 1 else 1
		current = BridgeIdentity.resolveOrdinalChild(current, tostring(pathSegments[i]), ordinal)
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

function BridgeIdentity.liveInstance(value: any): Instance?
	if typeof(value) == "Instance" and value.Parent ~= nil and value:IsDescendantOf(game) then
		return value
	end
	return nil
end

function BridgeIdentity.getDebugId(instance)
	local debugId = instance:GetDebugId(32)
	return if type(debugId) == "string" and #debugId > 0 then debugId else nil
end

local function siblingOrdinal(instance)
	local parent = instance.Parent
	if parent == nil or parent == game then
		return 1
	end
	local ordinal = 0
	for _, sibling in ipairs(parent:GetChildren()) do
		if sibling.Name == instance.Name then
			ordinal += 1
		end
		if sibling == instance then
			return ordinal
		end
	end
	return 1
end

function BridgeIdentity.getRefPathParts(instance)
	if instance == game then
		return {}, {}
	end
	if not instance:IsDescendantOf(game) then
		return nil, nil
	end

	local segments = {}
	local ordinals = {}
	local current = instance
	while current ~= nil and current ~= game do
		segments[#segments + 1] = current.Name
		ordinals[#ordinals + 1] = siblingOrdinal(current)
		current = current.Parent
	end
	for left = 1, math.floor(#segments / 2) do
		local right = #segments - left + 1
		segments[left], segments[right] = segments[right], segments[left]
		ordinals[left], ordinals[right] = ordinals[right], ordinals[left]
	end
	return segments, ordinals
end

function BridgeIdentity.getCachedRefPathParts(state, instance)
	if instance == game then
		return {}, {}
	end
	local cachedSegments = state.pathSegmentsByInstance[instance]
	local cachedOrdinals = state.pathOrdinalsByInstance[instance]
	if cachedSegments ~= nil and cachedOrdinals ~= nil then
		return cachedSegments, cachedOrdinals
	end
	if not instance:IsDescendantOf(game) then
		return nil, nil
	end

	local parent = instance.Parent
	local segments = {}
	local ordinals = {}
	if parent ~= nil and parent ~= game then
		local parentSegments, parentOrdinals = BridgeIdentity.getCachedRefPathParts(state, parent)
		if parentSegments == nil or parentOrdinals == nil then
			return nil, nil
		end
		for i, segment in ipairs(parentSegments) do
			segments[i] = segment
			ordinals[i] = parentOrdinals[i]
		end
	end
	segments[#segments + 1] = instance.Name
	ordinals[#ordinals + 1] = siblingOrdinal(instance)
	state.pathSegmentsByInstance[instance] = segments
	state.pathOrdinalsByInstance[instance] = ordinals
	return segments, ordinals
end

function BridgeIdentity.serializeRefValue(state, instance)
	if state ~= nil then
		local cachedInstanceId = state.instanceIdByInstance[instance]
		if type(cachedInstanceId) == "number" then
			cachedInstanceId = string.format("%x", cachedInstanceId)
			state.instanceIdByInstance[instance] = cachedInstanceId
		end
		if type(cachedInstanceId) == "string" and #cachedInstanceId > 0 then
			return {
				_type = "Ref",
				instanceId = cachedInstanceId,
			}
		end

		local pathSegments, pathOrdinals = BridgeIdentity.getCachedRefPathParts(state, instance)
		if pathSegments == nil or pathOrdinals == nil or #pathSegments == 0 then
			return nil
		end

		local out = {
			_type = "Ref",
			pathSegments = pathSegments,
			pathOrdinals = pathOrdinals,
		}
		local cachedDebugId = state.debugIdByInstance[instance]
		if cachedDebugId == nil then
			local debugId = BridgeIdentity.getDebugId(instance)
			if debugId ~= nil and #debugId > 0 then
				state.debugIdByInstance[instance] = debugId
				cachedDebugId = debugId
			else
				state.debugIdByInstance[instance] = false
				cachedDebugId = false
			end
		end
		if type(cachedDebugId) == "string" and #cachedDebugId > 0 then
			out.debugId = cachedDebugId
			local exported = state.exportedInstances and state.exportedInstances[instance]
			if not exported and state.isExportedInstance then
				exported = state.isExportedInstance(instance, pathSegments[1])
			end
			if #pathSegments > 1 and exported then
				out.settingsId = "debug:" .. cachedDebugId
			end
		end
		return out
	end

	local pathSegments, pathOrdinals = BridgeIdentity.getRefPathParts(instance)
	if pathSegments == nil or pathOrdinals == nil or #pathSegments == 0 then
		return nil
	end

	local out = {
		_type = "Ref",
		pathSegments = pathSegments,
		pathOrdinals = pathOrdinals,
	}
	local debugId = BridgeIdentity.getDebugId(instance)
	if debugId ~= nil and #debugId > 0 then
		out.debugId = debugId
	end
	return out
end

function BridgeIdentity.getCachedInstancePath(state, instance)
	local cached = state.pathByInstance[instance]
	if cached then
		return cached
	end
	local parent = instance.Parent
	local path = if parent == nil or parent == game
		then instance.Name
		else BridgeIdentity.getCachedInstancePath(state, parent) .. "." .. instance.Name
	state.pathByInstance[instance] = path
	return path
end

function BridgeIdentity.getCachedDebugId(state, instance)
	local cached = state.debugIdByInstance[instance]
	if cached ~= nil then
		if cached == false then
			return nil
		end
		return cached
	end

	local index = state.instanceIdByInstance[instance]
	local debugId = if type(index) == "number" and state.nativeDebugIds
		then state.nativeDebugIds[index]
		else BridgeIdentity.getDebugId(instance)
	if debugId then
		state.debugIdByInstance[instance] = debugId
		return debugId
	end
	state.debugIdByInstance[instance] = false
	return nil
end

function BridgeIdentity.getCachedInstanceId(state, instance)
	local cached = state.instanceIdByInstance[instance]
	if cached ~= nil then
		if cached == false then
			return nil
		end
		if type(cached) == "string" then
			return cached
		end
		if type(cached) == "number" then
			local instanceId = string.format("%x", cached)
			state.instanceIdByInstance[instance] = instanceId
			return instanceId
		end
	end
	state.instanceIdByInstance[instance] = false
	return nil
end

function BridgeIdentity.getCachedInstanceIndex(state, instance)
	local indexByInstance = state.instanceIndexByInstance
	if indexByInstance ~= nil then
		local cached = indexByInstance[instance]
		if cached ~= nil then
			return cached
		end
	end
	local cachedId = state.instanceIdByInstance[instance]
	if type(cachedId) == "number" then
		return cachedId
	end
	if type(cachedId) == "string" and cachedId ~= "" then
		return tonumber(cachedId, 16)
	end
	return nil
end

function BridgeIdentity.getCachedParentInstanceIndex(state, instance)
	local parent = instance.Parent
	if parent == nil or parent == game then
		return nil
	end
	return BridgeIdentity.getCachedInstanceIndex(state, parent)
end

function BridgeIdentity.parseInstanceIndexId(value)
	if type(value) == "number" then
		return value
	end
	if type(value) == "string" and value ~= "" then
		return tonumber(value, 16)
	end
	return nil
end

function BridgeIdentity.compactClassValue(state, className)
	local classId = state.classIdByName[className]
	if classId ~= nil then
		return classId
	end
	return className
end

function BridgeIdentity.getCachedScriptSourceKey(state, instance)
	local cached = state.scriptKeyByInstance[instance]
	if cached ~= nil then
		return cached
	end

	local instanceId = BridgeIdentity.getCachedInstanceId(state, instance)
	local key = if instanceId ~= nil and #instanceId > 0
		then "id:" .. instanceId
		else "path:" .. BridgeIdentity.getCachedInstancePath(state, instance)

	state.scriptKeyByInstance[instance] = key
	return key
end

return BridgeIdentity
