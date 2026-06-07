local BridgeStatus = {}

local function countChannels(channels)
	local openChannels = 0
	local connectingChannels = 0
	for _, channel in ipairs(channels or {}) do
		if channel.open then
			openChannels += 1
		elseif channel.connecting then
			connectingChannels += 1
		end
	end
	return openChannels, connectingChannels
end

local function formatClock(unix)
	if type(unix) ~= "number" or unix <= 0 then
		return "--"
	end
	return DateTime.fromUnixTimestamp(unix):FormatLocalTime("LTS", "en-us")
end

function BridgeStatus.view(state)
	local channels = state.channels or {}
	local openChannels, connectingChannels = countChannels(channels)
	local editor = state.editorSyncStats or {}
	local connectionStatus = tostring(state.connectionStatus or "Disconnected")
	local connectRequested = state.connectRequested == true

	local mode = "disconnected"
	if openChannels > 0 then
		mode = "connected"
	elseif connectRequested or connectingChannels > 0 or string.find(connectionStatus, "Connecting", 1, true) ~= nil then
		mode = "connecting"
	end

	local title = "Disconnected"
	local subtitle = connectionStatus
	if mode == "connected" then
		title = "Connected"
		subtitle = "Listening for editor and export sync requests."
	elseif mode == "connecting" then
		title = "Connecting..."
		subtitle = "Connecting to local sync server."
	end

	local lastSyncUnix = tonumber(editor.lastAtUnix) or 0
	local syncText = "Waiting for sync"
	if lastSyncUnix > 0 then
		if editor.lastOk == false then
			syncText = "Last sync failed at " .. formatClock(lastSyncUnix)
		else
			syncText = "Synced at " .. formatClock(lastSyncUnix)
		end
	elseif mode == "disconnected" then
		syncText = "Not connected"
	end

	local ports = state.ports or {}
	local address = tostring(state.host or "127.0.0.1") .. "  " .. table.concat(ports, ", ")
	local channelsText = ("%d/%d channels open, %d connecting"):format(openChannels, #channels, connectingChannels)
	local detailLines = {
		("Renium %s build %s"):format(tostring(state.bridgeVersion or "unknown"), tostring(state.bridgeBuildUnix or "unknown")),
		("Codec %s"):format(tostring(state.codecVersion or "unknown")),
		channelsText,
	}
	local statsLines = {
		("Editor requests %d | Last %.1f ms"):format(tonumber(editor.requests) or 0, tonumber(editor.lastMs) or 0),
		("Source +%d ~%d -%d | Instances +%d ~%d -%d"):format(
			tonumber(editor.sourceCreated) or 0,
			tonumber(editor.sourceUpdated) or 0,
			tonumber(editor.sourceDeleted) or 0,
			tonumber(editor.instanceCreated) or 0,
			tonumber(editor.instanceReplaced) or 0,
			tonumber(editor.instanceDeleted) or 0
		),
		("Properties %d | Attributes %d | No-op %d | Errors %d"):format(
			tonumber(editor.propertyUpdated) or 0,
			tonumber(editor.attributeUpdated) or 0,
			tonumber(editor.noops) or 0,
			tonumber(editor.errors) or 0
		),
	}

	return {
		mode = mode,
		title = title,
		subtitle = subtitle,
		connectionStatus = connectionStatus,
		syncText = syncText,
		address = address,
		channelsText = channelsText,
		detailText = table.concat(detailLines, "\n"),
		statsText = table.concat(statsLines, "\n"),
	}
end

function BridgeStatus.render(state)
	local view = BridgeStatus.view(state)
	return table.concat({
		view.title,
		view.subtitle,
		view.syncText,
		view.address,
		"",
		view.detailText,
		"",
		view.statsText,
	}, "\n")
end

return BridgeStatus
