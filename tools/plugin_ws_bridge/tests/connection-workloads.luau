local fs = require("@lune/fs")
local luau = require("@lune/luau")
local serde = require("@lune/serde")
local scheduler = require("@lune/task")

local function signal()
	local callbacks = {}
	local event = {}
	function event:Connect(callback)
		callbacks[callback] = true
		return { Disconnect = function() callbacks[callback] = nil end }
	end
	function event:Fire(...)
		for callback in table.clone(callbacks) do
			callback(...)
		end
	end
	function event:Wait()
		local thread = coroutine.running()
		local connection
		connection = self:Connect(function(...)
			connection:Disconnect()
			scheduler.spawn(thread, ...)
		end)
		return coroutine.yield()
	end
	function event:Destroy()
		table.clear(callbacks)
	end
	return event
end

local timers = {}
local destroyedSignals = 0
local Connection = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeConnection.module.lua"), {
	environment = {
		task = {
			spawn = scheduler.spawn,
			delay = function(seconds, callback)
				timers[#timers + 1] = { seconds = seconds, callback = callback }
			end,
		},
		Instance = { new = function(className)
			assert(className == "BindableEvent")
			local event = signal()
			return {
				Event = event,
				Fire = function() event:Fire() end,
				Destroy = function()
					event:Destroy()
					destroyedSignals += 1
				end,
			}
		end },
		Enum = { WebStreamClientType = { WebSocket = "WebSocket" } },
	},
})()

local function noop() end
local config = { bridgeConnectSession = 0, bridgeConnectRequested = false }
local sockets = {}
local failSend = false
local failMethod = false
local lastResponse
local settings = { autoConnect = false, autoReconnect = true, logLevel = "off" }
local context = {
	config = config,
	plugin = { GetSetting = noop, Unloading = signal() },
	ui = {
		hostBox = {}, portsBox = {},
		panelConnectButton = { MouseButton1Click = signal() },
		panelDisconnectButton = { MouseButton1Click = signal() },
		actions = { connect = { Triggered = signal() }, disconnect = { Triggered = signal() }, lock = { Triggered = signal() } },
		settingOptionButtons = {}, settingToggles = {}, settingInputs = {}, conflictOptionButtons = {},
		setRuntimeSettingActive = noop, setRuntimeSettingText = noop, setConflictResolutionActive = noop,
		setPlayModeHidden = noop, dismissNotification = noop,
	},
	settingsModule = { loadHost = function() return "127.0.0.1" end, loadPorts = function() return { 8781 } end, loadConflictResolution = noop },
	runtimeSettings = settings,
	settingsPrefix = "test",
	httpService = {
		JSONDecode = function(_, text) return serde.decode("json", text) end,
		CreateWebStreamClient = function()
			local socket = { Opened = signal(), Closed = signal(), Error = signal(), MessageReceived = signal(), closes = 0 }
			function socket:Close()
				self.closes += 1
				self.Closed:Fire()
			end
			sockets[#sockets + 1] = socket
			return socket
		end,
	},
	transportModule = {
		sendEnvelope = function() return not failSend, "test send failure" end,
		sendSuccessResponse = function(_, _, _, method, result)
			lastResponse = { method = method, result = result }
			return not failSend, "test send failure"
		end,
	},
	updateStatusText = noop, onRuntimeSettingsChanged = noop,
	acquireSessionLock = function() return true end,
	captureSessionLock = function() return 1 end,
	validateSessionLock = function() return true end,
	releaseSessionLock = noop,
	requestShutdown = function() return noop end,
	onRequestLeaseDisconnected = noop,
	isReplayProtectedMethod = function() return false end,
	isExclusiveMethod = function() return false end,
	isSessionOwnedMethod = function() return false end,
	handleMethod = function()
		if failMethod then error("test method failure", 0) end
		return { ok = true }
	end,
	connectSessionTimeoutSeconds = 8, fastReconnectWindowSeconds = 2,
	fastReconnectSeconds = 0.1, reconnectSeconds = 0.5,
	maxRequestBytes = 1024, maxQueuedExclusiveRequests = 16,
}
context.settingsModule.formatWebSocketUrl = function(host, port) return `ws://{host}:{port}` end
Connection.create(context)
assert(#sockets == 0, "autoConnect=false must not open a socket")
config.connectAll()
sockets[1].Opened:Fire()
assert(config.hasOpenChannel())
assert(not config.bridgeChannels[1].ready, "an open socket is not a registered runtime")
sockets[1].MessageReceived:Fire('{"id":1,"session_id":"new","method":"getBridgeInfo","params":{"registrationAck":true}}')
assert(lastResponse.method == "getBridgeInfo" and lastResponse.result.registrationAck == true,
	"the real handshake response must advertise the acknowledgement it waits for")
assert(not config.bridgeChannels[1].ready, "new daemon registration needs its acknowledgement")
sockets[1].MessageReceived:Fire('{"id":0,"session_id":"wrong","method":"bridgeRegistered","params":{}}')
assert(not config.bridgeChannels[1].ready)
sockets[1].MessageReceived:Fire('{"id":0,"session_id":"new","method":"bridgeRegistered","params":{}}')
assert(config.bridgeChannels[1].ready)

for _, failure in { "success-send", "error-send", "error-event", "closed-event" } do
	local socket = sockets[#sockets]
	table.clear(timers)
	failSend = true
	failMethod = failure == "error-send"
	if failure == "error-event" then
		socket.Error:Fire(0, "test connection failure")
	elseif failure == "closed-event" then
		socket.Closed:Fire()
	else
		socket.MessageReceived:Fire('{"id":1,"method":"test","params":{}}')
	end
	assert(not config.hasOpenChannel(), `{failure} must clear the dead channel`)
	assert(not config.bridgeChannels[1].ready, "reconnect must not retain the previous registration")
	assert(#timers == 1 and timers[1].seconds < 1, `{failure} must schedule one reconnect`)
	failSend = false
	timers[1].callback()
	sockets[#sockets].Opened:Fire()
	assert(config.hasOpenChannel(), `{failure} must recover`)
end
failMethod = false
sockets[#sockets].MessageReceived:Fire('{"id":2,"method":"getBridgeInfo","params":{}}')
assert(config.bridgeChannels[1].ready, "legacy daemons become ready after the successful handshake")

local returned = false
scheduler.spawn(function()
	local first, second, third = config.afterBridgeExclusiveIdle(function() return 42, nil, "done" end)
	assert(first == 42 and second == nil and third == "done")
	returned = true
end)
assert(returned, "already-idle cleanup must return without a lost wake")
local ok, message = pcall(config.afterBridgeExclusiveIdle, function() error("cleanup failed", 0) end)
assert(not ok and message == "cleanup failed")
local resume
returned = false
scheduler.spawn(function()
	config.afterBridgeExclusiveIdle(function()
		resume = coroutine.running()
		coroutine.yield()
	end)
	returned = true
end)
assert(not returned and resume ~= nil)
scheduler.spawn(resume)
assert(returned and destroyedSignals == 3, "yielded cleanup must wake and dispose its signal")

table.clear(timers)
config.disconnectAll("Disconnected")
assert(not config.hasOpenChannel() and not config.bridgeConnectRequested)
assert(#timers == 0, "intentional disconnect must not schedule a reconnect")
local Status = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeStatus.module.lua"), {
	environment = { DateTime = { fromUnixTimestamp = function()
		return { FormatLocalTime = function() return "12:00" end }
	end } },
})()
local state = {
	channels = { { open = true, ready = false } },
	editorSyncStats = setmetatable({ lastAtUnix = 100, lastOk = true }, {
		__index = function(_, key) return if key == "liveSync" then nil else 0 end,
	}),
	connectionStatus = "Registering Studio...", connectRequested = true, pendingEditCount = 42,
	pendingReviewCount = 0, host = "127.0.0.1", ports = { 8781 },
	bridgeVersion = "test", bridgeBuildUnix = 0, target = "Edit", runtimeId = "test", codecVersion = 1,
}
assert(Status.view(state).mode == "connecting")
state.channels[1].ready = true
local status = Status.view(state)
assert(status.mode == "connected" and status.subtitle == "42 Studio edits are waiting to sync.")
assert(status.syncText == "Last synced at 12:00")
state.pendingEditCount = 0
assert(Status.view(state).syncText == "Synced at 12:00")
print("Renium connection lifecycle tests passed")
