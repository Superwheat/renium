-- Deterministic scheduler: a stalled engine Async call never resumes itself.
local now = 0
local threads = {}
local mainThread = coroutine.running()
local originalClock = os.clock
os.clock = function() return now end
local stalledMethod = nil
local calls = {}
local device = "iphone_16_pro"
local authorized = true
task = {
	spawn = function(callback)
		local thread = coroutine.create(callback)
		threads[#threads + 1] = thread
		assert(coroutine.resume(thread))
		return thread
	end,
	cancel = function(thread) assert(coroutine.close(thread)) end,
}
local heartbeat = {
	Wait = function()
		if coroutine.running() ~= mainThread then
			coroutine.yield()
		else
			now += 0.1
		end
	end,
}
local service = setmetatable({}, { __index = function(_, name)
	return function(_, value)
		calls[#calls + 1] = name
		if name == stalledMethod then coroutine.yield() end
		if name == "GetDeviceAsync" then return device end
		if name == "GetDeviceListAsync" then return { "iphone_16_pro" } end
		if name == "GetDeviceInfoAsync" then
			return { DeviceId = value, Name = "iPhone 16 Pro", Width = 1206, Height = 2622 }
		end
		if name == "GetResolutionAsync" then return { X = 2622, Y = 1206 } end
		if name == "SetDeviceAsync" then device = value end
		if name == "StopSimulationAsync" then device = "default" end
		return 1
	end
end })
local services = {
	HttpService = { GenerateGUID = function() return "device-test" end },
	LogService = { MessageOut = { Connect = function() end }, GetLogHistory = function() return {} end },
	RunService = { RunState = "stopped", Heartbeat = heartbeat, IsRunning = function() return false end, IsEdit = function() return true end },
	StudioTestService = { EditModeActive = true },
	UserInputService = {},
	StudioDeviceSimulatorService = service,
}
game = { GetService = function(_, name) return assert(services[name], name) end }
workspace = { CurrentCamera = { ViewportSize = { X = 1206, Y = 2622 } } }
Enum = { RunState = { Stopped = "stopped" }, ScreenOrientation = {}, DeviceSimulatorScalingMode = {} }
script = { Parent = { BridgeValueCodec = {} } }
require = function(module) return module end

function runDeviceSimulatorWorkloads(RuntimeApi)
	local api = RuntimeApi.create({}, {
		runtimeId = "device-test", bridgeRole = "edit",
		assertRequestLeaseActive = function() assert(authorized, "cancelled lease") end,
		assertSessionOwnership = function() end,
	})
	for _, scenario in {
		{ "GetDeviceAsync", "status" },
		{ "GetResolutionAsync", "status" },
		{ "SetDeviceAsync", "set" },
		{ "StopSimulationAsync", "stop" },
	} do
		stalledMethod = scenario[1]
		device = "iphone_16_pro"
		local started = now
		local result = api.deviceSimulator({ action = scenario[2], device = "iPhone 16 Pro" })
		assert(not result.ok and result.timedOut and now - started < 1.7, `{scenario[1]}: {result.error}`)
		local retired = threads[#threads]
		assert(coroutine.status(retired) == "dead", "timed-out workers must not run later setters or rollback")
		stalledMethod = nil
		local recovered = api.deviceSimulator({ action = "status", waitForStartup = true, includeSettle = true })
		assert(recovered.ok and recovered.simulating and recovered.settleSeconds == 0,
			"a timeout must release startup settling and allow the next request")
	end
	assert(api.deviceSimulator({ action = "set", device = "iPhone 16 Pro" }).ok)
	assert(api.deviceSimulator({ action = "stop" }).ok)
	assert(not api.deviceSimulator({ action = "status" }).simulating)
	-- Some engine waits cannot be cancelled immediately. A late return must
	-- not run the next setter or rollback over a newer successful request.
	local cancel = task.cancel
	task.cancel = function() error("engine wait cannot be cancelled yet") end
	stalledMethod = "SetDeviceAsync"
	local expired = api.deviceSimulator({ action = "set", device = "iPhone 16 Pro", pixelDensity = 2 })
	assert(expired.timedOut)
	local late = threads[#threads]
	stalledMethod = nil
	assert(api.deviceSimulator({ action = "status", waitForStartup = true }).ok)
	local callsBefore = #calls
	assert(coroutine.resume(late))
	assert(#calls == callsBefore, "an expired worker must not call later setters or rollback")
	assert(api.deviceSimulator({ action = "status", includeSettle = true }).settleSeconds == 0)
	task.cancel = cancel
	stalledMethod = "GetDeviceAsync"
	local wait = heartbeat.Wait
	heartbeat.Wait = function() wait(); authorized = false end
	local result = api.deviceSimulator({ action = "status" })
	assert(not result.ok and not result.timedOut and string.find(result.error, "cancelled lease", 1, true))
	assert(coroutine.status(threads[#threads]) == "dead", "cancellation must retire the worker")
	os.clock = originalClock
end
