local BridgeIdentity = {}

function BridgeIdentity.getDebugId(instance)
	local debugId = instance:GetDebugId(32)
	return if type(debugId) == "string" and #debugId > 0 then debugId else nil
end

local function hashString32(text, seed)
	local hash = seed % 4294967296
	for i = 1, #text do
		local byte = string.byte(text, i)
		hash = (hash * 33 + byte) % 4294967296
	end
	return hash
end

function BridgeIdentity.shortenIdentifier(raw)
	local h1 = hashString32(raw, 5381)
	local h2 = hashString32(raw, 2166136261)
	return string.format("%08x%08x", h1, h2)
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
		table.insert(segments, 1, current.Name)
		table.insert(ordinals, 1, siblingOrdinal(current))
		current = current.Parent
	end
	return segments, ordinals
end

function BridgeIdentity.getRefPathSegments(instance)
	local segments = BridgeIdentity.getRefPathParts(instance)
	return segments
end

function BridgeIdentity.getRefPathOrdinals(instance)
	local _, ordinals = BridgeIdentity.getRefPathParts(instance)
	return ordinals
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

function BridgeIdentity.getCachedRefPathSegments(state, instance)
	local segments = BridgeIdentity.getCachedRefPathParts(state, instance)
	return segments
end

function BridgeIdentity.getCachedRefPathOrdinals(state, instance)
	local _, ordinals = BridgeIdentity.getCachedRefPathParts(state, instance)
	return ordinals
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

function BridgeIdentity.getCachedParentPath(state, instance)
	local parent = instance.Parent
	if parent == nil then
		return nil
	end
	if parent == game then
		return "game"
	end
	return BridgeIdentity.getCachedInstancePath(state, parent)
end

function BridgeIdentity.getCachedDebugId(state, instance)
	local cached = state.debugIdByInstance[instance]
	if cached ~= nil then
		if cached == false then
			return nil
		end
		return cached
	end

	local debugId = BridgeIdentity.getDebugId(instance)
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

function BridgeIdentity.getCachedParentDebugId(state, instance)
	local parent = instance.Parent
	if parent == nil or parent == game then
		return nil
	end
	return BridgeIdentity.getCachedDebugId(state, parent)
end

function BridgeIdentity.getCachedParentInstanceId(state, instance)
	local parent = instance.Parent
	if parent == nil or parent == game then
		return nil
	end
	return BridgeIdentity.getCachedInstanceId(state, parent)
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
