local BridgeStatus = {}

local function countChannels(channels)
	local openChannels = 0
	local connectingChannels = 0
	for _, channel in ipairs(channels) do
		if channel.open then
			openChannels += 1
		elseif channel.connecting then
			connectingChannels += 1
		end
	end
	return openChannels, connectingChannels
end

local function formatClock(unix)
	return DateTime.fromUnixTimestamp(unix):FormatLocalTime("LTS", "en-us")
end

function BridgeStatus.view(state)
	local channels = state.channels
	local openChannels, connectingChannels = countChannels(channels)
	local editor = state.editorSyncStats
	local connectionStatus = state.connectionStatus
	local connectRequested = state.connectRequested
	local pendingEditCount = state.pendingEditCount

	local mode = if openChannels > 0
		then "connected"
		elseif connectRequested or connectingChannels > 0 or string.find(connectionStatus, "Connecting", 1, true)
		then "connecting"
		else "disconnected"

	local title = if mode == "connected" then "Connected" elseif mode == "connecting" then "Connecting..." else "Disconnected"
	local subtitle = if mode == "disconnected" and pendingEditCount > 0
		then if pendingEditCount == 1 then "One Studio edit is waiting to sync." else `{pendingEditCount} Studio edits are waiting to sync.`
		elseif mode == "connected" or mode == "connecting" then ""
		elseif connectionStatus == "Disconnected" or connectionStatus == "Another Renium session is active" then ""
		else connectionStatus

	local lastSyncUnix = editor.lastAtUnix
	local syncText = if lastSyncUnix > 0
		then if editor.lastOk == false
			then "Last sync failed at " .. formatClock(lastSyncUnix)
			else "Synced at " .. formatClock(lastSyncUnix)
		elseif mode == "disconnected" then ""
		else "Waiting for sync"

	local address = state.host .. "  " .. table.concat(state.ports, ", ")
	local channelsText = ("%d/%d channels open, %d connecting"):format(openChannels, #channels, connectingChannels)
	local detailLines = {
		("Renium %s build %s"):format(state.bridgeVersion, state.bridgeBuildUnix),
		("Target %s | Runtime %s"):format(state.target, state.runtimeId),
		("Codec %s"):format(state.codecVersion),
		channelsText,
		("Pending Studio edits %d"):format(pendingEditCount),
		("Pending reviews %d"):format(state.pendingReviewCount),
	}
	local statsLines = {
		("Editor requests %d | Last %.1f ms"):format(editor.requests, editor.lastMs),
		("Source +%d ~%d -%d | Instances +%d ~%d -%d"):format(
			editor.sourceCreated,
			editor.sourceUpdated,
			editor.sourceDeleted,
			editor.instanceCreated,
			editor.instanceReplaced,
			editor.instanceDeleted
		),
		("Properties %d | Attributes %d | No-op %d | Errors %d"):format(
			editor.propertyUpdated,
			editor.attributeUpdated,
			editor.noops,
			editor.errors
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

return BridgeStatus
