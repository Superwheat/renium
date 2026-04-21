local BridgeStatus = {}

function BridgeStatus.render(state)
	local lines = {
		("Enabled: %s"):format(state.enabled and "ON" or "OFF"),
		("Host: %s"):format(state.host),
		("Ports: %s"):format(table.concat(state.ports, ", ")),
		("ExportAllProperties: %s"):format(state.exportAllProperties and "ON" or "OFF"),
		("PreSerialize: %s%s"):format(
			state.preSerializeOnPrepare and "ON" or "OFF",
			state.preSerializeInstanceThreshold and (" (<=%d)"):format(state.preSerializeInstanceThreshold) or ""
		),
		"",
	}
	for _, channel in ipairs(state.channels) do
		local status = "CLOSED"
		if channel.open then
			status = "OPEN"
		elseif channel.connecting then
			status = "CONNECTING"
		end
		table.insert(lines, ("Channel %d (%d): %s"):format(channel.id, channel.port, status))
	end
	return table.concat(lines, "\n")
end

return BridgeStatus
