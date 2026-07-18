

local BridgeConnection = {}

function BridgeConnection.create(context)
	local plugin = context.plugin
	local Config = context.config
	local ui = context.ui
	local SettingsModule = context.settingsModule
	local TransportModule = context.transportModule
	local HttpService = context.httpService
	local RunService = context.runService

	local host = SettingsModule.loadHost(plugin, context.settingsPrefix, context.defaultHost)
	local ports = SettingsModule.loadPorts(plugin, context.settingsPrefix, context.defaultPorts)
	local runtimeSettings = SettingsModule.loadRuntimeSettings(plugin, context.settingsPrefix)
	local channels = {}
	local connectChannel
	local prepareChannelsForNextRun
	local reconnectWatchdogStarted = false
	local pluginUnloading = false
	local maxReconnectSeconds = math.max(tonumber(context.maxReconnectSeconds) or 4.0, tonumber(context.reconnectSeconds) or 0.5)
	local stableConnectionSeconds = math.max(tonumber(context.stableConnectionSeconds) or 1.0, 0)
	local maxRequestBytes = math.max(1024, tonumber(context.maxRequestBytes) or 16 * 1024 * 1024)
	local maxQueuedExclusiveRequests = math.max(1, tonumber(context.maxQueuedExclusiveRequests) or 16)
	local exclusiveRequestBusy = false
	local exclusiveRequestQueue = {}
	local logLevelRank = {
		off = 0,
		error = 1,
		warn = 2,
		info = 3,
		debug = 4,
		trace = 5,
	}

	local function autoReconnectEnabled(): boolean
		return runtimeSettings.autoReconnect ~= false
	end

	local function bridgeLogEnabled(level: string): boolean
		local configured = tostring(runtimeSettings.logLevel or "warn")
		return (logLevelRank[configured] or logLevelRank.warn) >= (logLevelRank[level] or logLevelRank.warn)
	end

	local function bridgeLog(level: string, message: string)
		if bridgeLogEnabled(level) then
			warn("[Renium] " .. message)
		end
	end

	local function applyRuntimeSettingsToUi()
		if ui.setRuntimeSettingActive ~= nil then
			for key, value in pairs(runtimeSettings) do
				ui.setRuntimeSettingActive(key, value)
			end
		end
		if ui.setRuntimeSettingText ~= nil then
			ui.setRuntimeSettingText("changesThreshold", runtimeSettings.changesThreshold)
			ui.setRuntimeSettingText("diffLinesLimit", runtimeSettings.diffLinesLimit)
		end
	end

	local function notifyRuntimeSettingsChanged()
		Config.bridgeSettings = runtimeSettings
		if type(context.onRuntimeSettingsChanged) == "function" then
			context.onRuntimeSettingsChanged(runtimeSettings)
		end
	end

	function Config.getBridgeSettings()
		local copy = {}
		for key, value in pairs(runtimeSettings) do
			copy[key] = value
		end
		return copy
	end

	function Config.setBridgeSetting(key, value)
		local normalized = SettingsModule.saveRuntimeSetting(plugin, context.settingsPrefix, key, value)
		if normalized == nil then
			return nil
		end
		runtimeSettings[key] = normalized
		applyRuntimeSettingsToUi()
		notifyRuntimeSettingsChanged()
		return normalized
	end

	local function syncConfigState()
		Config.bridgeHost = host
		Config.bridgePorts = ports
		Config.bridgeChannels = channels
	end

	local function updateStatusText()
		syncConfigState()
		context.updateStatusText()
	end

	local function debugBridgeConnection(message)
		if context.debugBridgeConnection or bridgeLogEnabled("debug") then
			bridgeLog("debug", "bridge debug: " .. message)
		end
	end

	local function conciseConnectionError(reason)
		local text = tostring(reason or "connection failed")
		text = string.gsub(text, "[%c\r\n]+", " ")
		if #text > 140 then
			text = string.sub(text, 1, 137) .. "..."
		end
		return text
	end

	local function markConnectionFailure(channel, reason)
		channel.lastError = conciseConnectionError(reason)
		if Config.bridgeConnectRequested and not Config.bridgePausedForPlay then
			if autoReconnectEnabled() then
				Config.bridgeConnectionStatus = "Connecting..."
			else
				Config.bridgeConnectionStatus = ("Connection failed on channel %d: %s"):format(channel.id, channel.lastError)
			end
		end
	end

	local function captureChannelClients()
		local snapshot = table.create(#channels)
		for i, channel in ipairs(channels) do
			snapshot[i] = channel.client
		end
		return snapshot
	end

	local function sendRequestError(channelId, client, id, message)
		TransportModule.sendEnvelope(client, {
			id = id,
			ok = false,
			error = message,
			channel = channelId,
		})
	end

	local function executeRequest(channel, client, id, method, params)
		if pluginUnloading or channel.client ~= client then
			return
		end
		local started = os.clock()
		local okCall, result = pcall(context.handleMethod, method, params)
		local serverMs = (os.clock() - started) * 1000
		if okCall then
			local sent, sendError = TransportModule.sendSuccessResponse(channel.id, client, id, method, result, serverMs)
			if not sent then
				local responseError = conciseConnectionError(sendError or "could not send bridge response")
				local errorSent = TransportModule.sendEnvelope(client, {
					id = id,
					ok = false,
					error = responseError,
					channel = channel.id,
					timings = {
						serverMs = serverMs,
					},
				})
				if not errorSent then
					pcall(function()
						client:Close()
					end)
				end
				return
			end
			if method == "prepareForNextRun" then
				local channelClients = captureChannelClients()
				task.delay(context.nextRunCloseDelaySeconds, function()
					if prepareChannelsForNextRun ~= nil then
						prepareChannelsForNextRun(channelClients)
					end
				end)
			end
		else
			TransportModule.sendEnvelope(client, {
				id = id,
				ok = false,
				error = tostring(result),
				channel = channel.id,
				timings = {
					serverMs = serverMs,
				},
			})
		end
	end

	local drainExclusiveRequestQueue
	drainExclusiveRequestQueue = function()
		if exclusiveRequestBusy then
			return
		end
		local request = table.remove(exclusiveRequestQueue, 1)
		if request == nil then
			return
		end
		exclusiveRequestBusy = true
		task.spawn(function()
			executeRequest(request.channel, request.client, request.id, request.method, request.params)
			exclusiveRequestBusy = false
			drainExclusiveRequestQueue()
		end)
	end

	local function onMessage(channel, client, message)
		local channelId = channel.id
		if type(message) ~= "string" then
			sendRequestError(channelId, client, nil, "Bridge request must be text")
			return
		end
		if #message > maxRequestBytes then
			sendRequestError(channelId, client, nil, "Bridge request exceeds safe size limit")
			return
		end
		local okDecode, payload = pcall(function()
			return HttpService:JSONDecode(message)
		end)
		if not okDecode or type(payload) ~= "table" then
			sendRequestError(channelId, client, nil, "Invalid JSON payload")
			return
		end

		local id = payload.id
		if type(id) ~= "number" or id < 0 or id ~= math.floor(id) then
			sendRequestError(channelId, client, nil, "Bridge request has an invalid id")
			return
		end
		local method = payload.method
		if type(method) ~= "string" or method == "" or #method > 96 then
			sendRequestError(channelId, client, id, "Missing or invalid bridge method")
			return
		end
		if type(context.allowedMethods) == "table" and context.allowedMethods[method] ~= true then
			sendRequestError(channelId, client, id, "Unsupported bridge method")
			return
		end
		local params = payload.params
		if params ~= nil and type(params) ~= "table" then
			sendRequestError(channelId, client, id, "Bridge request params must be an object")
			return
		end
		if type(context.isExclusiveMethod) == "function" and context.isExclusiveMethod(method) then
			if #exclusiveRequestQueue >= maxQueuedExclusiveRequests then
				sendRequestError(channelId, client, id, "Bridge mutation queue is full; retry shortly")
				return
			end
			table.insert(exclusiveRequestQueue, {
				channel = channel,
				client = client,
				id = id,
				method = method,
				params = params,
			})
			drainExclusiveRequestQueue()
			return
		end
		task.spawn(executeRequest, channel, client, id, method, params)
	end

	local function scheduleReconnect(channel)
		if pluginUnloading or Config.bridgePausedForPlay or not Config.bridgeConnectRequested or not channel.shouldReconnect then
			return
		end
		if not autoReconnectEnabled() then
			channel.shouldReconnect = false
			channel.reconnectScheduled = false
			if not Config.hasOpenChannel() then
				Config.bridgeConnectRequested = false
				Config.bridgeConnectedOnce = false
				Config.bridgeConnectDeadline = 0
				Config.bridgeConnectionStatus = "Disconnected (auto reconnect is off)"
				updateStatusText()
			end
			return
		end
		if channel.reconnectScheduled then
			return
		end
		local now = os.clock()
		local channelCount = math.max(#channels, 1)
		local failures = math.max(0, tonumber(channel.reconnectFailureCount) or 0)
		local period = tonumber(context.reconnectSeconds) or 0.5
		if failures == 0 and channel.fastReconnectUntil > now then
			period = context.fastReconnectSeconds
		end
		if failures > 0 then
			local backoffPower = math.min(failures - 1, 4)
			local backoff = math.max(tonumber(context.reconnectSeconds) or 0.5, 0.1) * (2 ^ backoffPower)
			period = math.max(period, math.min(maxReconnectSeconds, backoff))
		end
		local target = channel.forcedReconnectAt
		if target ~= nil and target > 0 then
			channel.forcedReconnectAt = 0
		else
			local phase = ((channel.id - 1) % channelCount) * (period / channelCount)
			target = channel.nextReconnectAt
			if target <= 0 then
				target = now + phase
			elseif target <= now + 0.005 then
				target += (math.floor((now - target) / period) + 1) * period
			elseif target - now > period then
				target = now + phase
			end
		end
		channel.nextReconnectAt = target
		channel.reconnectScheduled = true
		task.delay(math.max(0.005, target - now), function()
			if pluginUnloading or not Config.bridgeConnectRequested or not channel.shouldReconnect or not autoReconnectEnabled() then
				channel.reconnectScheduled = false
				return
			end
			channel.reconnectScheduled = false
			if channel.client ~= nil then
				return
			end
			connectChannel(channel)
		end)
	end

	local function recordReconnectFailure(channel)
		channel.reconnectFailureCount = math.min((tonumber(channel.reconnectFailureCount) or 0) + 1, 8)
	end

	local function recordReconnectClose(channel, wasOpen)
		if not wasOpen then
			recordReconnectFailure(channel)
			return
		end
		local openedAt = tonumber(channel.openedAt) or 0
		if openedAt <= 0 or os.clock() - openedAt < stableConnectionSeconds then
			recordReconnectFailure(channel)
		end
	end

	local function resetReconnectFailures(channel)
		channel.reconnectFailureCount = 0
	end

	local function closeChannel(channel)
		local wasOpen = channel.open
		if channel.client ~= nil then
			debugBridgeConnection(
				("closeChannel channel %d wasOpen=%s shouldReconnect=%s connecting=%s"):format(
					channel.id,
					tostring(wasOpen),
					tostring(channel.shouldReconnect),
					tostring(channel.connecting)
				)
			)
		end
		channel.connecting = false
		channel.open = false
		channel.reconnectScheduled = false
		if wasOpen then
			channel.fastReconnectUntil = os.clock() + context.fastReconnectWindowSeconds
		end
		local client = channel.client
		channel.client = nil
		if client then
			pcall(function()
				client:Close()
			end)
		end
		if not pluginUnloading then
			updateStatusText()
		end
	end

	function Config.hasOpenChannel()
		for _, channel in ipairs(channels) do
			if channel.open then
				return true
			end
		end
		return false
	end

	function Config.hasAllOpenChannels()
		if #channels == 0 then
			return false
		end
		for _, channel in ipairs(channels) do
			if not channel.open then
				return false
			end
		end
		return true
	end

	function Config.disconnectAll(reason)
		Config.bridgeConnectRequested = false
		Config.bridgeConnectedOnce = false
		Config.bridgeConnectSession += 1
		Config.bridgeConnectDeadline = 0
		Config.bridgeConnectionStatus = reason or "Disconnected"
		for _, channel in ipairs(channels) do
			channel.shouldReconnect = false
			closeChannel(channel)
		end
		updateStatusText()
	end

	function Config.disconnectIfConnectionInterrupted()
		if not autoReconnectEnabled() then
			return false
		end
		if not Config.bridgePausedForPlay and Config.bridgeConnectRequested and Config.bridgeConnectedOnce and not Config.hasOpenChannel() then
			Config.bridgeConnectionStatus = "Connecting..."
			Config.bridgeConnectedOnce = false
			Config.bridgeConnectDeadline = os.clock() + context.connectSessionTimeoutSeconds
			for _, channel in ipairs(channels) do
				if channel.client == nil and not channel.connecting then
					channel.shouldReconnect = true
					channel.reconnectScheduled = false
				end
			end
			updateStatusText()
		end
		return false
	end

	local function keepFastReconnectIfNextRunActive(channel)
		local now = os.clock()
		if channel.nextRunFastUntil > now then
			channel.fastReconnectUntil = now + context.fastReconnectWindowSeconds
		end
	end

	connectChannel = function(channel)
		if pluginUnloading or Config.bridgePausedForPlay or not Config.bridgeConnectRequested or not channel.shouldReconnect then
			return
		end
		local now = os.clock()
		if Config.bridgeConnectDeadline > 0 and now >= Config.bridgeConnectDeadline and not Config.bridgeConnectedOnce then
			Config.bridgeConnectDeadline = 0
			markConnectionFailure(channel, "connection timed out")
			updateStatusText()
		end
		if channel.client ~= nil or channel.connecting then
			return
		end

		channel.reconnectScheduled = false
		channel.connecting = true
		channel.open = false
		channel.connectAttempt += 1
		local attempt = channel.connectAttempt
		local openTimeoutSeconds = context.connectOpenTimeoutSeconds
		if channel.fastReconnectUntil > now then
			openTimeoutSeconds = context.fastConnectOpenTimeoutSeconds
		end
		if channel.nextRunFastUntil > now then
			openTimeoutSeconds = context.nextRunConnectTimeoutSeconds
		end
		if Config.bridgeConnectDeadline > now then
			openTimeoutSeconds = math.max(openTimeoutSeconds, Config.bridgeConnectDeadline - now)
		end
		if not pluginUnloading then
			updateStatusText()
		end

		local url = SettingsModule.formatWebSocketUrl(host, channel.port)
		local ok, client = pcall(function()
			return HttpService:CreateWebStreamClient(Enum.WebStreamClientType.WebSocket, {
				Url = url,
			})
		end)

		if not ok or not client then
			channel.connecting = false
			channel.open = false
			recordReconnectFailure(channel)
			markConnectionFailure(channel, client or "could not create WebSocket client")
			keepFastReconnectIfNextRunActive(channel)
			if not pluginUnloading then
				updateStatusText()
			end
			scheduleReconnect(channel)
			return
		end

		channel.client = client
		channel.open = false
		task.delay(openTimeoutSeconds, function()
			if pluginUnloading then
				return
			end
			if channel.client ~= client or channel.open or channel.connectAttempt ~= attempt then
				return
			end
			channel.client = nil
			channel.connecting = false
			channel.open = false
			recordReconnectFailure(channel)
			markConnectionFailure(channel, "connection timed out")
			keepFastReconnectIfNextRunActive(channel)
			if not pluginUnloading then
				updateStatusText()
			end
			pcall(function()
				client:Close()
			end)
			scheduleReconnect(channel)
		end)

		client.Opened:Connect(function(_statusCode, _headers)
			if pluginUnloading then
				pcall(function()
					client:Close()
				end)
				return
			end
			if channel.client ~= client then
				return
			end
			channel.connecting = false
			channel.open = true
			channel.openedAt = os.clock()
			channel.reconnectScheduled = false
			channel.shouldReconnect = Config.bridgeConnectRequested
			resetReconnectFailures(channel)
			channel.lastError = nil
			debugBridgeConnection(("channel %d opened attempt=%d"):format(channel.id, attempt))
			channel.nextRunFastUntil = 0
			channel.fastReconnectUntil = os.clock() + context.fastReconnectWindowSeconds
			Config.bridgeConnectedOnce = true
			Config.bridgeConnectionStatus = "Connected"
			updateStatusText()
			TransportModule.sendEnvelope(client, {
				id = nil,
				ok = true,
				event = "hello",
				channel = channel.id,
				version = context.bridgeVersion,
				bridgeVersion = context.bridgeVersion,
				runtimeId = context.runtimeId,
				bridgeRole = Config.bridgeRole,
				protocolVersion = context.protocolVersion,
				codecVersion = context.codecVersion,
				bridgeBuildUnix = context.bridgeBuildUnix,
				chunkFrameProtocolVersion = context.chunkFrameProtocolVersion,
				chunkSliceBudgetKb = 437,
				compactValueProtocolVersion = context.compactValueProtocolVersion,
				largeServiceWarmMode = context.preSerializeLargeServiceWarm and "coordinated" or "disabled",
				serializerWorkerMode = context.serializerWorkerMode,
			})
		end)

		client.MessageReceived:Connect(function(message)
			if pluginUnloading or channel.client ~= client then
				return
			end
			onMessage(channel, client, message)
		end)

		client.Error:Connect(function(_statusCode, _errorMessage)
			if pluginUnloading then
				return
			end
			if channel.client ~= client then
				return
			end
			local wasOpen = channel.open
			debugBridgeConnection(
				("channel %d error wasOpen=%s shouldReconnect=%s error=%s"):format(
					channel.id,
					tostring(wasOpen),
					tostring(channel.shouldReconnect),
					tostring(_errorMessage)
				)
			)
			channel.client = nil
			channel.connecting = false
			channel.open = false
			channel.reconnectScheduled = false
			recordReconnectClose(channel, wasOpen)
			markConnectionFailure(channel, _errorMessage or "WebSocket error")
			if wasOpen then
				channel.fastReconnectUntil = os.clock() + context.fastReconnectWindowSeconds
			else
				keepFastReconnectIfNextRunActive(channel)
			end
			updateStatusText()
			if not Config.disconnectIfConnectionInterrupted() then
				scheduleReconnect(channel)
			end
			pcall(function()
				client:Close()
			end)
		end)

		client.Closed:Connect(function()
			local wasOpen = channel.open
			debugBridgeConnection(
				("channel %d closed wasOpen=%s shouldReconnect=%s clientMatch=%s"):format(
					channel.id,
					tostring(wasOpen),
					tostring(channel.shouldReconnect),
					tostring(channel.client == client)
				)
			)
			if channel.client == client then
				channel.client = nil
			end
			if channel.client ~= nil then
				return
			end
			channel.connecting = false
			channel.open = false
			recordReconnectClose(channel, wasOpen)
			if not wasOpen and Config.bridgeConnectRequested then
				markConnectionFailure(channel, "connection closed before opening")
			end
			if wasOpen then
				channel.fastReconnectUntil = os.clock() + context.fastReconnectWindowSeconds
			else
				keepFastReconnectIfNextRunActive(channel)
			end
			if not pluginUnloading then
				updateStatusText()
			end
			if not pluginUnloading and not Config.disconnectIfConnectionInterrupted() then
				scheduleReconnect(channel)
			end
		end)

		if not pluginUnloading then
			updateStatusText()
		end
	end

	local function ensureReconnectWatchdog()
		if pluginUnloading or reconnectWatchdogStarted then
			return
		end
		reconnectWatchdogStarted = true
		task.spawn(function()
			local lastPauseCheck = 0
			while not pluginUnloading do
				task.wait(0.05)
				local now = os.clock()
				if Config.bridgePausedForPlay and now - lastPauseCheck >= 1 then
					lastPauseCheck = now
					if not Config.isPlayModeActiveForBridge() then
						Config.setPausedForPlay(false)
					end
				end
				if Config.bridgeConnectRequested and not Config.bridgePausedForPlay then
					if Config.bridgeConnectDeadline > 0 and os.clock() >= Config.bridgeConnectDeadline and not Config.bridgeConnectedOnce then
						Config.bridgeConnectDeadline = 0
						if channels[1] ~= nil then
							markConnectionFailure(channels[1], "connection timed out")
						else
							Config.bridgeConnectionStatus = "Connecting..."
						end
						updateStatusText()
					end
					for _, channel in ipairs(channels) do
						if channel.shouldReconnect and channel.client == nil and not channel.connecting and not channel.reconnectScheduled then
							connectChannel(channel)
						end
					end
				end
			end
		end)
	end

	function Config.connectAll()
		if pluginUnloading or Config.bridgePausedForPlay then
			return
		end
		Config.bridgeConnectRequested = true
		Config.bridgeConnectedOnce = false
		Config.bridgeConnectSession += 1
		Config.bridgeConnectDeadline = os.clock() + context.connectSessionTimeoutSeconds
		local session = Config.bridgeConnectSession
		Config.bridgeConnectionStatus = "Connecting..."
		ensureReconnectWatchdog()
		for _, channel in ipairs(channels) do
			channel.shouldReconnect = true
			closeChannel(channel)
			connectChannel(channel)
		end
		task.delay(context.connectSessionTimeoutSeconds, function()
			if pluginUnloading or session ~= Config.bridgeConnectSession or not Config.bridgeConnectRequested then
				return
			end
			if Config.bridgeConnectedOnce and Config.hasOpenChannel() then
				Config.bridgeConnectDeadline = 0
				for _, channel in ipairs(channels) do
					channel.shouldReconnect = Config.bridgeConnectRequested
					channel.reconnectScheduled = false
				end
			else
				Config.bridgeConnectDeadline = 0
				if channels[1] ~= nil then
					markConnectionFailure(channels[1], "connection timed out")
				else
					Config.bridgeConnectionStatus = "Connecting..."
				end
				for _, channel in ipairs(channels) do
					channel.shouldReconnect = Config.bridgeConnectRequested
					if channel.client == nil and not channel.connecting and not channel.reconnectScheduled then
						scheduleReconnect(channel)
					end
				end
				updateStatusText()
			end
		end)
		updateStatusText()
	end

	function Config.setPausedForPlay(paused)
		if Config.startedInPlayMode then
			return
		end
		if Config.bridgePausedForPlay == paused then
			return
		end
		Config.bridgePausedForPlay = paused
		if paused then
			Config.bridgeConnectSession += 1
			Config.bridgeConnectDeadline = 0
			Config.bridgeConnectionStatus = "Paused during play"
			for _, channel in ipairs(channels) do
				channel.shouldReconnect = false
				channel.reconnectScheduled = false
				closeChannel(channel)
			end
			updateStatusText()
			return
		end

		Config.bridgeConnectionStatus = "Connecting..."
		for _, channel in ipairs(channels) do
			channel.shouldReconnect = Config.bridgeConnectRequested
			channel.reconnectScheduled = false
			channel.nextReconnectAt = 0
		end
		if Config.bridgeConnectRequested then
			Config.connectAll()
		else
			updateStatusText()
		end
	end

	prepareChannelsForNextRun = function(channelClients)
		if pluginUnloading or Config.bridgePausedForPlay or not Config.bridgeConnectRequested then
			return
		end
		ensureReconnectWatchdog()
		local now = os.clock()
		local channelCount = math.max(#channels, 1)
		for i, channel in ipairs(channels) do
			channel.shouldReconnect = true
			channel.nextRunFastUntil = now + context.nextRunFastWindowSeconds
			channel.fastReconnectUntil = now + context.nextRunFastWindowSeconds
			local phase = (i - 1) * (context.fastReconnectSeconds / channelCount)
			channel.forcedReconnectAt = now + context.nextRunReconnectDelaySeconds + phase
			if channel.client == channelClients[i] then
				closeChannel(channel)
				scheduleReconnect(channel)
			elseif channel.client == nil and not channel.connecting and not channel.reconnectScheduled then
				scheduleReconnect(channel)
			end
		end
	end

	function Config.resetChannels()
		for _, channel in ipairs(channels) do
			channel.shouldReconnect = false
			closeChannel(channel)
		end
		table.clear(channels)
		local now = os.clock()
		local channelCount = math.max(#ports, 1)
		for i, port in ipairs(ports) do
			channels[i] = {
				id = i,
				port = port,
				client = nil,
				open = false,
				connecting = false,
				reconnectScheduled = false,
				shouldReconnect = Config.bridgeConnectRequested,
				connectAttempt = 0,
				reconnectFailureCount = 0,
				nextReconnectAt = now + ((i - 1) * (context.reconnectSeconds / channelCount)),
				fastReconnectUntil = 0,
				forcedReconnectAt = 0,
				nextRunFastUntil = 0,
				openedAt = 0,
			}
		end
		syncConfigState()
	end

	function Config.parsePortsCsv(raw)
		return SettingsModule.parsePortsCsv(raw)
	end

	function Config.applyWidgetSettings(reconnectIfRequested)
		local typedHost = string.gsub(ui.hostBox.Text or "", "^%s*(.-)%s*$", "%1")
		if typedHost == "" then
			typedHost = context.defaultHost
		end
		local nextHost = SettingsModule.normalizeLoopbackHost(typedHost)
		if nextHost == nil then
			bridgeLog("warn", "bridge host must be loopback (127.0.0.1 or ::1)")
			ui.hostBox.Text = host
			return false
		end

		local parsedPorts = Config.parsePortsCsv(ui.portsBox.Text or "")
		if parsedPorts == nil then
			bridgeLog("warn", "invalid ports list, expected comma-separated numbers")
			ui.portsBox.Text = table.concat(ports, ",")
			return false
		end

		host = nextHost
		ports = parsedPorts
		if not SettingsModule.saveHostPorts(plugin, context.settingsPrefix, host, ports) then
			bridgeLog("warn", "refused to save a non-loopback bridge host")
			ui.hostBox.Text = host
			return false
		end

		Config.resetChannels()
		updateStatusText()
		if reconnectIfRequested ~= false and Config.bridgeConnectRequested then
			Config.connectAll()
		end
		return true
	end

	syncConfigState()
	ui.hostBox.Text = host
	ui.portsBox.Text = table.concat(ports, ",")
	notifyRuntimeSettingsChanged()
	applyRuntimeSettingsToUi()

	ui.panelConnectButton.MouseButton1Click:Connect(function()
		if pluginUnloading then
			return
		end
		if Config.applyWidgetSettings(false) then
			Config.connectAll()
		end
	end)
	ui.panelDisconnectButton.MouseButton1Click:Connect(function()
		if pluginUnloading then
			return
		end
		Config.disconnectAll("Disconnected")
	end)

	if ui.settingOptionButtons ~= nil then
		for setting, buttons in pairs(ui.settingOptionButtons) do
			for rawValue, optionButton in pairs(buttons) do
				optionButton.MouseButton1Click:Connect(function()
					if pluginUnloading then
						return
					end
					local value: any = rawValue
					if rawValue == "true" then
						value = true
					elseif rawValue == "false" then
						value = false
					end
					Config.setBridgeSetting(setting, value)
				end)
			end
		end
	end

	if ui.settingToggles ~= nil then
		for setting, toggle in pairs(ui.settingToggles) do
			toggle.button.MouseButton1Click:Connect(function()
				if pluginUnloading then
					return
				end
				Config.setBridgeSetting(setting, not toggle.get())
			end)
		end
	end

	if ui.settingInputs ~= nil then
		for setting, input in pairs(ui.settingInputs) do
			input.FocusLost:Connect(function()
				if pluginUnloading then
					return
				end
				local normalized = Config.setBridgeSetting(setting, input.Text)
				if normalized == nil then
					applyRuntimeSettingsToUi()
				end
			end)
		end
	end

	if ui.conflictOptionButtons ~= nil then
		for value, optionButton in pairs(ui.conflictOptionButtons) do
			optionButton.MouseButton1Click:Connect(function()
				if pluginUnloading then
					return
				end
				if Config.studioChanges ~= nil then
					Config.studioChanges.setConflictResolution(value)
				end
				SettingsModule.saveConflictResolution(plugin, context.settingsPrefix, value)
				if ui.setConflictResolutionActive ~= nil then
					ui.setConflictResolutionActive(value)
				end
			end)
		end
		if ui.setConflictResolutionActive ~= nil then
			ui.setConflictResolutionActive(
				SettingsModule.loadConflictResolution(plugin, context.settingsPrefix, nil)
			)
		end
	end

	function Config.updatePlayModeBridgeState()
		local playModeActive = Config.isPlayModeActiveForBridge()
		ui.setPlayModeHidden(playModeActive)
		Config.setPausedForPlay(playModeActive)
	end

	pcall(function()
		(game:GetService("StudioTestService") :: any):GetPropertyChangedSignal("EditModeActive"):Connect(Config.updatePlayModeBridgeState)
	end)
	pcall(function()
		RunService:GetPropertyChangedSignal("RunState"):Connect(Config.updatePlayModeBridgeState)
	end)
	Config.updatePlayModeBridgeState()

	plugin.Unloading:Connect(function()
		pluginUnloading = true
		if type(context.onUnload) == "function" then
			pcall(context.onUnload)
		end
		for _, channel in ipairs(channels) do
			channel.shouldReconnect = false
			closeChannel(channel)
		end
		table.clear(channels)
		syncConfigState()
	end)

	Config.resetChannels()
	updateStatusText()
	if runtimeSettings.autoConnect ~= false then
		Config.connectAll()
	end

	return {
		getHost = function()
			return host
		end,
		getPorts = function()
			return ports
		end,
		getChannels = function()
			return channels
		end,
	}
end

return BridgeConnection
