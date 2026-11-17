local fs = require("@lune/fs")
local luau = require("@lune/luau")
local now, waits = 0, 0
local parallel = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeParallel.module.luau"), {
	environment = {
		os = { clock = function() return now end },
		task = { wait = function() waits += 1; now += 0.1 end },
	},
})()
for _, slice in ipairs({ 0.008, 0.032 }) do
	local start = now
	local budget = parallel.newWorkBudget(if slice == 0.008 then nil else slice)
	now = start + slice - 0.001
	assert(not budget(), "A short slice must remain synchronous")
	local before = waits
	now = start + slice + 0.001
	assert(budget() and waits == before + 1, "Long work must yield at its chosen deadline")
	assert(not budget(), "Scheduler wait time must not consume the next work slice")
end
print("work budgets retain bounded scheduler cooperation")
