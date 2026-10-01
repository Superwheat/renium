local fs = require("@lune/fs")
local luau = require("@lune/luau")
local scheduler = require("@lune/task")

local Fake = {}
Fake.__index = function(instance, key)
	local props = rawget(instance, "props")
	if props[key] ~= nil then
		return props[key]
	end
	return Fake[key]
end
Fake.__newindex = function(instance, key, value)
	local props = rawget(instance, "props")
	if key == "Parent" then
		local previous = props.Parent
		if previous ~= nil then
			local siblings = rawget(previous, "children")
			table.remove(siblings, table.find(siblings, instance))
		end
		if value ~= nil then
			table.insert(rawget(value, "children"), instance)
		end
	end
	props[key] = value
	if key == "Enabled" and value == true and props.Source ~= nil then
		local run = luau.load(props.Source, {
			environment = {
				script = instance,
				task = scheduler,
			},
		})
		scheduler.spawn(run)
	end
end
function Fake:GetChildren()
	return table.clone(rawget(self, "children"))
end
function Fake:FindFirstChild(name)
	for _, child in rawget(self, "children") do
		if child.Name == name then
			return child
		end
	end
	return nil
end
function Fake:Destroy()
	rawget(self, "props").Destroyed = true
	self.Parent = nil
	for _, child in table.clone(rawget(self, "children")) do
		child:Destroy()
	end
end
function Fake:Fire(...)
	for _, handler in rawget(self, "handlers") do
		handler(...)
	end
end
function Fake:Invoke(...)
	local props = rawget(self, "props")
	assert(not props.Destroyed, "cannot invoke a destroyed BindableFunction")
	return props.OnInvoke(...)
end
local function newInstance(className)
	local instance = setmetatable({
		props = { Name = className, ClassName = className },
		children = {},
		handlers = {},
	}, Fake)
	if className == "BindableEvent" then
		rawget(instance, "props").Event = {
			Connect = function(_, handler)
				local handlers = rawget(instance, "handlers")
				handlers[#handlers + 1] = handler
				return {
					Disconnect = function()
						local index = table.find(handlers, handler)
						if index then
							table.remove(handlers, index)
						end
					end,
				}
			end,
		}
	end
	return instance
end

local serverScriptService = newInstance("Folder")
serverScriptService.Name = "ServerScriptService"
local services = {
	HttpService = { GenerateGUID = function() return "test-runtime" end },
	LogService = {
		MessageOut = { Connect = function() return { Disconnect = function() end } end },
		GetLogHistory = function() return {} end,
	},
	UserInputService = {},
	RunService = {
		RunState = "running",
		IsEdit = function() return false end,
		IsRunning = function() return true end,
		IsServer = function() return true end,
	},
	StudioTestService = { EditModeActive = false, CanLeaveTest = function() return false end },
	ServerScriptService = serverScriptService,
}
local runtimeGame = {
	GetService = function(_, name) return assert(services[name], name) end,
	GetAttribute = function() return nil end,
	SetAttribute = function() end,
}
local codec = {
	decodeNumber = function(value) return tonumber(value) end,
	encodeNumber = function(value) return value end,
	encodeComponents = function(...) return { ... } end,
}
local RuntimeApi = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeRuntimeApi.module.luau"), {
	environment = {
		game = runtimeGame,
		Enum = { RunState = { Stopped = "stopped" }, RunContext = { Server = "Server" } },
		Instance = { new = newInstance },
		task = scheduler,
		script = { Parent = { BridgeValueCodec = codec } },
		require = function(module)
			assert(module == codec)
			return module
		end,
	},
})()
local api = RuntimeApi.create({}, {
	runtimeId = "test-runtime",
	bridgeRole = "play-server",
	assertRequestLeaseActive = function() end,
	assertSessionOwnership = function() end,
})

local function runnerCount()
	return #serverScriptService:GetChildren()
end

local plain = api.executeLuau({ code = "return 1 + 1", context = "server" })
assert(plain.ok and plain.results[1] == 2 and plain.detached == nil, "plain runner result")
assert(runnerCount() == 0, "a plain runner is removed on return")

local immediate = api.executeLuau({
	code = "state.count = 0\ntask.spawn(function()\n\tfor _ = 1, 3 do\n\t\tstate.count += 1\n\t\ttask.wait()\n\tend\nend)\nreturn 'started'",
	context = "server",
	detach = "counter",
	lifetimeSeconds = 5,
})
assert(immediate.ok and immediate.detached == "counter" and immediate.running == false, "a body that returns at once reports its result")
assert(immediate.results[1] == "started", "detached result values")
assert(runnerCount() == 1, "a detached runner stays")
scheduler.wait(0.2)
local collected = api.collectLuauRunner({ name = "counter" })
assert(collected.ok and collected.done and collected.status == "ok", "collect reads a finished body")
assert(collected.state.count == 3, "threads spawned by a detached runner keep running: " .. tostring(collected.state.count))
assert(collected.results[1] == "started", "collect repeats the final results")

local yielding = api.executeLuau({
	code = "print('tick')\nfor i = 1, 3 do\n\tstate.n = i\n\ttask.wait()\nend\nreturn 'done'",
	context = "server",
	detach = "loop",
	lifetimeSeconds = 5,
})
assert(yielding.ok and yielding.detached == "loop" and yielding.running == true, "a yielding body returns detached")
assert(yielding.results == nil and yielding.output[1].message == "tick", "output captured before the first yield is returned")
assert(runnerCount() == 2, "two detached runners coexist")
scheduler.wait(0.2)
local finished = api.collectLuauRunner({ name = "loop", stop = true })
assert(finished.ok and finished.done and finished.results[1] == "done" and finished.state.n == 3, "collect after completion")
assert(finished.stopped and runnerCount() == 1, "stop removes the runner")
local missing = api.collectLuauRunner({ name = "loop" })
assert(not missing.ok and string.find(missing.error, "counter", 1, true), "a missing name lists the running runners: " .. tostring(missing.error))

local replaced = api.executeLuau({ code = "return 'again'", context = "server", detach = "counter", lifetimeSeconds = 5 })
assert(replaced.ok and runnerCount() == 1, "re-detaching a name replaces the previous runner")

local failing = api.executeLuau({
	code = "task.wait()\nerror('boom')",
	context = "server",
	detach = "failing",
	lifetimeSeconds = 5,
})
assert(failing.ok and failing.running == true, "a body that fails after yielding is still detached")
scheduler.wait(0.1)
local failure = api.collectLuauRunner({ name = "failing" })
assert(failure.ok and failure.done and failure.status == "error" and string.find(failure.results[1], "boom", 1, true), "collect reports the body's error")

services.RunService.IsRunning = function() return false end
local edit = api.executeLuau({ code = "return 1", context = "plugin", detach = "edit" })
assert(not edit.ok and string.find(edit.error, "Play", 1, true), "detaching needs Play")

api.requestCancellation()
assert(runnerCount() == 0, "cancellation removes detached runners")
print("Renium detached runner tests passed")
