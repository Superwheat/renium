local BridgeSettings = {}

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
	if type(value) == "string" and value ~= "" then
		return value
	end
	return defaultHost
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
			-- Auto-upgrade legacy default port sets to the current default lanes.
			if #valid == 3 and valid[1] == 8781 and valid[2] == 8782 and valid[3] == 8783 and #defaultPorts >= 4 then
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
				and #defaultPorts == 4 then
				return defaultPorts
			end
			return valid
		end
	end
	return defaultPorts
end

function BridgeSettings.loadEnabled(plugin, prefix, defaultEnabled)
	local value = plugin:GetSetting(prefix .. "enabled")
	if type(value) == "boolean" then
		return value
	end
	return defaultEnabled
end

function BridgeSettings.saveHostPorts(plugin, prefix, host, ports)
	plugin:SetSetting(prefix .. "host", host)
	plugin:SetSetting(prefix .. "ports", ports)
end

function BridgeSettings.saveEnabled(plugin, prefix, enabled)
	plugin:SetSetting(prefix .. "enabled", enabled == true)
end

return BridgeSettings
