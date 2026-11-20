local fs = require("@lune/fs")
local luau = require("@lune/luau")
local now, waits, frame = 0, 0, 0.1
local parallel = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeParallel.module.luau"), {
	environment = {
		os = { clock = function() return now end },
		task = { wait = function() waits += 1; now += frame end },
		math = math,
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
local observed = 0
local budget = parallel.newWorkBudget(nil, function(started)
	observed += 1
	assert(math.abs(now - started - 0.1) < 0.00001)
end)
assert(not budget() and observed == 0)
now += 0.009
assert(budget() and observed == 1)
for _, case in ipairs({ { 0.003, 0.0303 }, { 0.02, 0.02 }, { 0.2, 0.1 } }) do
	frame = case[1]
	local adaptive = parallel.newWorkBudget()
	now += 0.009
	assert(adaptive(), "The first slice is the shortest")
	now += case[2] - 0.001
	assert(not adaptive(), "A slice lasts what the frame has left, or as long as Studio's own time took")
	now += 0.002
	assert(adaptive(), "A slice ends when the frame is spent")
end
frame = 0.003
local fixed = parallel.newWorkBudget(0.032)
now += 0.033
assert(fixed())
now += 0.031
assert(not fixed(), "A caller's own slice stays as given")
-- A request's budget: fast frames leave the 0.25 s floor, a slow frame sets
-- the next slice, and a 3 s autosave stall still ends the next one at 1 s.
for _, case in ipairs({ { 0.016, 0.25 }, { 0.6, 0.6 }, { 3, 1 } }) do
	frame = case[1]
	local budget, charged = parallel.newRequestBudget(), 0
	now += 0.25 - 0.001
	assert(not budget(), "Work shorter than the first slice never yields")
	now += 0.002
	assert(budget(function() charged += 1 end) and charged == 1, "A spent slice yields once, charged to its caller")
	now += case[2] - 0.001
	assert(not budget(), `After a {case[1]} s wait the work yielded before {case[2]} s`)
	now += 0.002
	assert(budget(), `After a {case[1]} s wait the work held Studio past {case[2]} s`)
end
print("work budgets retain bounded scheduler cooperation")
