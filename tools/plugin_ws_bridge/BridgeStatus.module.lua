local BridgeStatus = {}

local function countChannels(channels)
	local openChannels = 0
	local readyChannels = 0
	local connectingChannels = 0
	for _, channel in ipairs(channels) do
		if channel.open then
			openChannels += 1
			if channel.ready then
				readyChannels += 1
			end
		elseif channel.connecting then
			connectingChannels += 1
		end
	end
	return openChannels, connectingChannels, readyChannels
end

local function formatClock(unix)
	return DateTime.fromUnixTimestamp(unix):FormatLocalTime("LTS", "en-us")
end

function BridgeStatus.view(state)
	local channels = state.channels
	local openChannels, connectingChannels, readyChannels = countChannels(channels)
	local editor = state.editorSyncStats
	local connectionStatus = state.connectionStatus
	local connectRequested = state.connectRequested
	local pendingEditCount = state.pendingEditCount
	local liveSync = editor.liveSync
	local syncFailed = liveSync ~= nil and type(liveSync.error) == "string" and liveSync.error ~= ""

	local mode = if readyChannels > 0
		then "connected"
		elseif
			connectRequested
			or openChannels > 0
			or connectingChannels > 0
			or string.find(connectionStatus, "Connecting", 1, true)
		then "connecting"
		else "disconnected"

	local title = if mode == "connected"
		then "Connected"
		elseif mode == "connecting" then "Connecting..."
		else "Disconnected"
	local subtitle = if pendingEditCount > 0
		then if pendingEditCount == 1
			then "One Studio edit is waiting to sync."
			else `{pendingEditCount} Studio edits are waiting to sync.`
		elseif mode == "connected" or mode == "connecting" then ""
		elseif connectionStatus == "Disconnected" or connectionStatus == "Another Renium session is active" then ""
		else connectionStatus
	if mode == "connected" and liveSync ~= nil then
		if syncFailed then
			title = "Sync failed"
			subtitle = "Changes are still pending. See the editor or rbx lst for details."
		elseif liveSync.resolutionRequired then
			title = "Sync needs a decision"
			subtitle = "Resolve the conflict in the editor or CLI."
		elseif not liveSync.running then
			subtitle = "Live Sync is off."
		elseif liveSync.paused then
			subtitle = "Live Sync is paused. Changes will wait."
		elseif liveSync.readOnly then
			subtitle = "Verify mode: changes are not applied."
		end
	end

	local lastSyncUnix = editor.lastAtUnix
	local syncText = if lastSyncUnix > 0
		then if editor.lastOk == false
			then "Last sync failed at " .. formatClock(lastSyncUnix)
			elseif pendingEditCount > 0 then "Last synced at " .. formatClock(lastSyncUnix)
			else "Synced at " .. formatClock(lastSyncUnix)
		elseif mode == "disconnected" then ""
		else "Waiting for sync"

	local address = state.host .. "  " .. table.concat(state.ports, ", ")
	local channelsText = `{readyChannels}/{#channels} channels ready, {openChannels} open, {connectingChannels} connecting`
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
		syncFailed = syncFailed,
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
