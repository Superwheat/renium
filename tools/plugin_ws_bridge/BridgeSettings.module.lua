local BridgeSettings = {}

local RUNTIME_DEFAULTS = {
	autoConnect = true,
	autoReconnect = true,
	liveHydrate = true,
	keepUnknowns = false,
	twoWaySync = true,
	syncbackProperties = true,
	onlyCodeMode = false,
	initialSyncPriority = "studio",
	diffLinesLimit = 3000,
	displayPrompts = "always",
	changesThreshold = 5,
	logLevel = "warn",
	overridePackages = false,
}

local RUNTIME_BOOLEAN_KEYS = {
	autoConnect = true,
	autoReconnect = true,
	liveHydrate = true,
	keepUnknowns = true,
	twoWaySync = true,
	syncbackProperties = true,
	onlyCodeMode = true,
	overridePackages = true,
}

local RUNTIME_ENUMS = {
	initialSyncPriority = {
		studio = true,
		editor = true,
		none = true,
	},
	displayPrompts = {
		always = true,
		initial = true,
		never = true,
	},
	logLevel = {
		off = true,
		error = true,
		warn = true,
		info = true,
		debug = true,
		trace = true,
	},
}

local RUNTIME_NUMBERS = {
	changesThreshold = { minimum = 0, maximum = 100000 },
	diffLinesLimit = { minimum = 100, maximum = 1000000 },
}

local function trim(value)
	return string.gsub(tostring(value or ""), "^%s*(.-)%s*$", "%1")
end

function BridgeSettings.normalizeLoopbackHost(raw)
	local host = string.lower(trim(raw))
	if host == "" or host == "localhost" or host == "127.0.0.1" then
		return "127.0.0.1"
	end
	if host == "[::1]" or host == "::1" then
		return "::1"
	end
	return nil
end

function BridgeSettings.formatWebSocketUrl(host, port)
	if host == "::1" then
		return string.format("ws://[::1]:%d", port)
	end
	return string.format("ws://%s:%d", host, port)
end

function BridgeSettings.parsePortsCsv(raw)
	local out = {}
	local seen = {}
	for piece in string.gmatch(raw or "", "[^,]+") do
		local text = string.gsub(piece, "^%s*(.-)%s*$", "%1")
		local num = tonumber(text)
		if num == nil then
			return nil
		end
		local port = math.floor(num)
		if port <= 0 or port > 65535 then
			return nil
		end
		if not seen[port] then
			seen[port] = true
			table.insert(out, port)
		end
	end
	if #out == 0 then
		return nil
	end
	return out
end

function BridgeSettings.loadHost(plugin, prefix, defaultHost)
	local value = plugin:GetSetting(prefix .. "host")
	if type(value) == "string" then
		local normalized = BridgeSettings.normalizeLoopbackHost(value)
		if normalized ~= nil then
			return normalized
		end
	end
	return BridgeSettings.normalizeLoopbackHost(defaultHost) or "127.0.0.1"
end

function BridgeSettings.loadPorts(plugin, prefix, defaultPorts)
	local configuredPorts = plugin:GetSetting(prefix .. "ports")
	if type(configuredPorts) == "table" and #configuredPorts > 0 then
		local valid = {}
		local seen = {}
		for _, value in ipairs(configuredPorts) do
			if type(value) == "number" and value > 0 then
				local port = math.floor(value)
				if port <= 65535 and not seen[port] then
					seen[port] = true
					table.insert(valid, port)
				end
			end
		end
		if #valid > 0 then
			if
				#valid == 4
				and valid[1] == 8781
				and valid[2] == 8782
				and valid[3] == 8783
				and valid[4] == 8784
				and #defaultPorts == 3
			then
				return defaultPorts
			end
			if
				#valid == 3
				and valid[1] == 8781
				and valid[2] == 8782
				and valid[3] == 8783
				and #defaultPorts == 4
				and defaultPorts[1] == 8781
				and defaultPorts[2] == 8782
				and defaultPorts[3] == 8783
				and defaultPorts[4] == 8784
			then
				return defaultPorts
			end
			if #valid == 8
				and valid[1] == 8781
				and valid[2] == 8782
				and valid[3] == 8783
				and valid[4] == 8784
				and valid[5] == 8785
				and valid[6] == 8786
				and valid[7] == 8787
				and valid[8] == 8788
				and (#defaultPorts == 3 or #defaultPorts == 4) then
				return defaultPorts
			end
			return valid
		end
	end
	return defaultPorts
end

function BridgeSettings.saveHostPorts(plugin, prefix, host, ports)
	local normalizedHost = BridgeSettings.normalizeLoopbackHost(host)
	if normalizedHost == nil then
		return false
	end
	plugin:SetSetting(prefix .. "host", normalizedHost)
	plugin:SetSetting(prefix .. "ports", ports)
	return true
end

function BridgeSettings.runtimeDefaults()
	local copy = {}
	for key, value in pairs(RUNTIME_DEFAULTS) do
		copy[key] = value
	end
	return copy
end

function BridgeSettings.normalizeRuntimeSetting(key, value)
	if RUNTIME_BOOLEAN_KEYS[key] then
		if type(value) == "boolean" then
			return value
		end
		return RUNTIME_DEFAULTS[key]
	end

	local enum = RUNTIME_ENUMS[key]
	if enum ~= nil then
		local normalized = string.lower(trim(value))
		if enum[normalized] then
			return normalized
		end
		return RUNTIME_DEFAULTS[key]
	end

	local numberSettings = RUNTIME_NUMBERS[key]
	if numberSettings ~= nil then
		local numeric = tonumber(value)
		if numeric == nil or numeric ~= numeric then
			return RUNTIME_DEFAULTS[key]
		end
		return math.clamp(math.floor(numeric), numberSettings.minimum, numberSettings.maximum)
	end

	return nil
end

function BridgeSettings.loadRuntimeSettings(plugin, prefix)
	local out = BridgeSettings.runtimeDefaults()
	for key in pairs(out) do
		local stored = plugin:GetSetting(prefix .. key)
		if stored ~= nil then
			local normalized = BridgeSettings.normalizeRuntimeSetting(key, stored)
			if normalized ~= nil then
				out[key] = normalized
			end
		end
	end
	return out
end

function BridgeSettings.saveRuntimeSetting(plugin, prefix, key, value)
	local normalized = BridgeSettings.normalizeRuntimeSetting(key, value)
	if normalized == nil then
		return nil
	end
	plugin:SetSetting(prefix .. key, normalized)
	return normalized
end

function BridgeSettings.loadConflictResolution(plugin, prefix, default)
	local value = plugin:GetSetting(prefix .. "conflictResolution")
	if value == "prompt" or value == "filesystem" or value == "studio" then
		return value
	end
	return default
end

function BridgeSettings.saveConflictResolution(plugin, prefix, value)
	if value == "prompt" or value == "filesystem" or value == "studio" then
		plugin:SetSetting(prefix .. "conflictResolution", value)
	end
end

return BridgeSettings
