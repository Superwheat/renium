local fs = require("@lune/fs")
local luau = require("@lune/luau")
local scheduler = require("@lune/task")

-- Console readers written against earlier plugin builds name their cursor
-- `since` or `cursorSeq`; both keep answering like `sinceSeq`.
local messageOut
local services = {
	HttpService = { GenerateGUID = function() return "test-runtime" end },
	LogService = {
		MessageOut = {
			Connect = function(_, callback)
				messageOut = callback
				return { Disconnect = function() end }
			end,
		},
		GetLogHistory = function()
			return {
				{ message = "first", messageType = { Name = "MessageOutput" } },
				{ message = "second", messageType = { Name = "MessageOutput" } },
			}
		end,
	},
	UserInputService = {},
	RunService = {
		RunState = "stopped",
		IsEdit = function() return true end,
		IsRunning = function() return false end,
	},
	StudioTestService = { EditModeActive = true, CanLeaveTest = function() return false end },
}
local runtimeGame = {
	GetService = function(_, name) return assert(services[name], name) end,
	GetAttribute = function() return nil end,
	SetAttribute = function() end,
}
local codec = {}
local RuntimeApi = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeRuntimeApi.module.luau"), {
	environment = {
		game = runtimeGame,
		Enum = { RunState = { Stopped = "stopped" } },
		task = scheduler,
		script = { Parent = { BridgeValueCodec = codec } },
		require = function(module)
			assert(module == codec)
			return module
		end,
	},
})()
local api = RuntimeApi.create({}, {
	runtimeId = "test-runtime", bridgeRole = "edit",
	assertRequestLeaseActive = function() end,
	assertSessionOwnership = function() end,
})
assert(messageOut, "the console recorder subscribes to LogService.MessageOut")
scheduler.wait()
messageOut("third", { Name = "MessageOutput" })
messageOut("fourth", { Name = "MessageWarning" })

local all = api.getConsoleOutput({ fromOldest = true })
assert(#all.entries == 4, `four console entries, got {#all.entries}`)
assert(all.entries[1].message == "first" and all.entries[4].message == "fourth", "entries keep their order")
local cursor = all.entries[2].seq
assert(cursor > 0 and all.entries[1].seq == cursor - 1, "sequence numbers are consecutive")

local function messages(result)
	local texts = {}
	for _, entry in ipairs(result.entries) do
		texts[#texts + 1] = entry.message
	end
	return table.concat(texts, ",")
end

local reference = api.getConsoleOutput({ sinceSeq = cursor })
assert(messages(reference) == "third,fourth", `sinceSeq reads the newer entries, got {messages(reference)}`)
for _, name in { "since", "cursorSeq" } do
	local result = api.getConsoleOutput({ [name] = cursor })
	assert(messages(result) == messages(reference), `{name} reads the same entries as sinceSeq, got {messages(result)}`)
	assert(result.nextSeq == reference.nextSeq, `{name} answers the same cursor`)
	assert(result.truncated == reference.truncated and result.hasMore == reference.hasMore)
end
local none = api.getConsoleOutput({ cursorSeq = reference.nextSeq })
assert(#none.entries == 0 and none.nextSeq == reference.nextSeq, "a cursor at the newest entry reads nothing")
api.cleanup()
