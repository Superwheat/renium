local BridgeSettings = {}
local MAX_BRIDGE_PORTS = 2

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

local function normalizePorts(values, maximumCount)
	if type(values) ~= "table" then
		return nil
	end
	local count = 0
	for key in pairs(values) do
		if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then
			return nil
		end
		count += 1
	end
	if count < 1 or count ~= #values then
		return nil
	end
	local out = table.create(math.min(count, maximumCount))
	local seen = {}
	for _, port in ipairs(values) do
		if
			type(port) ~= "number"
			or port ~= port
			or port % 1 ~= 0
			or port < 1
			or port > 65535
		then
			return nil
		end
		if not seen[port] then
			if #out >= maximumCount then
				return nil
			end
			seen[port] = true
			out[#out + 1] = port
		end
	end
	return out
end

local function isDefaultPortSequence(values)
	if #values < 2 or #values > MAX_BRIDGE_PORTS then
		return false
	end
	for index, port in ipairs(values) do
		if port ~= 8780 + index then
			return false
		end
	end
	return true
end

function BridgeSettings.normalizeLoopbackHost(raw)
	local host = string.lower(trim(raw))
	if host == "" or host == "localhost" or host == "127.0.0.1" then
		return "127.0.0.1"
	elseif host == "[::1]" or host == "::1" then
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
	for piece in string.gmatch(raw or "", "[^,]+") do
		local text = string.gsub(piece, "^%s*(.-)%s*$", "%1")
		local num = tonumber(text)
		if not num then
			return nil
		end
		out[#out + 1] = num
	end
	return normalizePorts(out, MAX_BRIDGE_PORTS)
end

function BridgeSettings.loadHost(plugin, prefix, defaultHost)
	local value = plugin:GetSetting(prefix .. "host")
	if type(value) == "string" then
		local normalized = BridgeSettings.normalizeLoopbackHost(value)
		if normalized then
			return normalized
		end
	end
	return BridgeSettings.normalizeLoopbackHost(defaultHost) or "127.0.0.1"
end

function BridgeSettings.loadPorts(plugin, prefix, defaultPorts)
	local configuredPorts = plugin:GetSetting(prefix .. "ports")
	local valid = normalizePorts(configuredPorts, MAX_BRIDGE_PORTS)
	if valid then
		if isDefaultPortSequence(valid) and isDefaultPortSequence(defaultPorts) then
			return defaultPorts
		end
		return valid
	end
	return defaultPorts
end

function BridgeSettings.saveHostPorts(plugin, prefix, host, ports)
	local normalizedHost = BridgeSettings.normalizeLoopbackHost(host)
	local normalizedPorts = normalizePorts(ports, MAX_BRIDGE_PORTS)
	if not normalizedHost or not normalizedPorts then
		return false
	end
	plugin:SetSetting(prefix .. "host", normalizedHost)
	plugin:SetSetting(prefix .. "ports", normalizedPorts)
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
	if enum then
		local normalized = string.lower(trim(value))
		if enum[normalized] then
			return normalized
		end
		return RUNTIME_DEFAULTS[key]
	elseif RUNTIME_NUMBERS[key] then
		local numberSettings = RUNTIME_NUMBERS[key]
		local numeric = tonumber(value)
		if not numeric or numeric ~= numeric then
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
