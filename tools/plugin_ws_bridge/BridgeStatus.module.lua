local BridgeStatus = {}

function BridgeStatus.render(state)
	local lines = {
		("Host: %s"):format(state.host),
		("Ports: %s"):format(table.concat(state.ports, ", ")),
		("ExportAllProperties: %s"):format(state.exportAllProperties and "ON" or "OFF"),
		("PreSerialize: %s"):format(state.preSerializeOnPrepare and "ON" or "OFF"),
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
