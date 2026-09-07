local Network = require("../BridgeNetworkSimulation.module")
local names = {
	"InboundNetworkMinDelayMs", "OutboundNetworkMinDelayMs", "InboundNetworkJitterMs",
	"OutboundNetworkJitterMs", "InboundNetworkLossPercent", "OutboundNetworkLossPercent",
}
local values = {}
for _, name in names do values[name] = 0 end
values.IncomingReplicationLag = 0.125
local writes = 0
local reject = nil
local clamp = nil
local settings = setmetatable({}, {
	__index = values,
	__newindex = function(_, name, value)
		writes += 1
		if reject == name then error("permission denied") end
		values[name] = if clamp == name then math.min(value, 5) else value
	end,
})
local api = Network.create(function() return settings end)
local shown = api.handle({ action = "show" })
assert(shown.settings.inDelay == 0 and writes == 0)
for _, params in {
	{ action = "set", values = {} },
	{ action = "set", values = { inDelay = 25, outLoss = 0.51 } },
	{ action = "set", values = { inDelay = 0 / 0 } },
	{ action = "set", values = { inDelay = math.huge } },
	{ action = "set", values = { inDelay = -1 } },
	{ action = "set", values = { inDelay = "10" } },
	{ action = "show", values = { inDelay = 10 } },
	{ action = "set", values = { unknown = 10 } },
} do
	assert(not pcall(api.handle, params))
	assert(writes == 0, "invalid input must not partially change settings")
end
for index = 1, 1000 do
	local delay = index % 101
	local result = api.handle({ action = "set", client = true, values = { inDelay = delay, outLoss = 0.25 } })
	assert(result.settings.inDelay == delay and result.settings.outLoss == 0.25)
	assert(result.settings.outDelay == 0 and values.IncomingReplicationLag == 0.125)
end
api.restore()
assert(values.InboundNetworkMinDelayMs == 0 and values.OutboundNetworkLossPercent == 0)
local count = writes
api.handle({ action = "set", values = { inDelay = 0 } })
assert(writes == count, "matching settings must remain a no-op")

for _, mode in { "permission", "clamp" } do
	reject = if mode == "permission" then "OutboundNetworkMinDelayMs" else nil
	clamp = if mode == "clamp" then "OutboundNetworkMinDelayMs" else nil
	assert(not pcall(api.handle, { action = "set", client = true, values = { inDelay = 40, outDelay = 40 } }))
	assert(values.InboundNetworkMinDelayMs == 0 and values.OutboundNetworkMinDelayMs == 0)
end
reject, clamp = nil, nil
api.handle({ action = "set", client = true, values = { inDelay = 20, outDelay = 30 } })
values.OutboundNetworkMinDelayMs = 15 -- Later manual change does not belong to Renium.
api.handle({ action = "restore", client = true })
assert(values.InboundNetworkMinDelayMs == 0 and values.OutboundNetworkMinDelayMs == 15)
api.handle({ action = "reset" })
for _, name in names do assert(values[name] == 0) end
assert(values.IncomingReplicationLag == 0.125)
print("Network simulation: validation, 1000 rapid changes, rollback, no-op and ownership-aware restoration passed")
