local analyze = require("../../renium/src/studio/automation/microprofiler_analysis")
local disposed, iterDisposed = 0, 0
local currentFrame, step = 0, 0
local session = {
	IsValid = function() return true end,
	GetFrameIdMin = function() return 1 end,
	GetFrameIdMax = function() return 3 end,
	FetchGlobalDesc = function() return { TickToMsCpu = 1, TickToMsGpu = 0.5 } end,
	FetchFrameDesc = function(_, id)
		return { FrameAbsoluteId = if id == 1 then 10 else 98 + id, TickStartCpu = id * 100,
			TickEndCpu = id * 100 + (if id == 1 then 100 else 10), TickStartGpu = 0, TickEndGpu = 0 }
	end,
	FetchThreadDesc = function() return { ThreadName = "Main", IsGpu = false } end,
	FetchTimerDesc = function() return { TimerName = "Work", GroupId = 1 } end,
	FetchGroupDesc = function() return { GroupName = "Physics" } end,
	FetchCounterIds = function() return { 1, 2 } end,
	FetchCounterDesc = function(_, id) return { CounterName = if id == 1 then "memory" else "heap", Parent = if id == 1 then 0 else 1 } end,
	FetchLastCounterSample = function(_, id) return { Value = id * 2, FrameId = 3 } end,
	FindTimerIds = function(_, filter) assert(filter == "Work") return { 1 } end,
	FindThreadIds = function(_, filter) assert(filter == "Main") return { 1 } end,
	FindCounterIds = function(_, filter) assert(filter == "**/heap") return { 2 } end,
	Dispose = function() disposed += 1 end,
}
local element = { EnterTimerId = function() return 1 end, EnterTimestampFrameLocal = function() return currentFrame * 100 + (if step == 1 then -5 else 8) end }
local state = {
	ThreadStackIsOverflowed = function() return false end,
	ThreadStackWasOverflowed = function() return false end,
	ThreadStackIsUnderflowed = function() return false end,
	IsThreadFrameEnd = function() return step == 2 end,
	IsExit = function() return step == 1 end,
	ThreadStackDepth = function() return if step == 1 then 0 else 1 end,
	ThreadId = function() return 1 end,
	TimerId = function() return 1 end,
	Timestamp = function() return currentFrame * 100 + (if step == 1 then 5 else 15) end,
}
function session:CreateLogIterator()
	return {
		Configure = function(_, config) assert(config.EmitThreadFrameEnd and config.SkipEvents) end,
		RewindTo = function(_, first, last) assert(first == last) currentFrame = first step = 0 end,
		GetState = function() return state end,
		Step = function() step += 1 return step <= 2 end,
		GetCurrentThreadStackElement = function() return element end,
		Dispose = function() iterDisposed += 1 end,
	}
end
local lib = { Session = { OpenFromBuffer = function() return session end } }
local result = analyze(lib, buffer.create(1), nil, 1)
assert(result.frameCount == 3 and result.measuredFrameCount == 2 and result.statisticsSegment == 2)
assert(result.cpuMs.mean == 10 and result.cpuMs.max == 10, "Old stitched captures must not inflate current statistics")
assert(#result.frames == 2 and result.frames[1].scopes[1].inclusiveMs == 7, "Only the portions inside this frame count")
assert(result.frames[1].scopes[1].spans == 2)
assert(result.frames[1].scopes[1].wallMs == 7, "Disjoint calls retain their full covered duration")
assert(result.counterCount == 2 and #result.counters == 1)
assert(disposed == 1 and iterDisposed == 2)
local filtered = analyze(lib, buffer.create(1), 2, 5, "Work", "Main", "**/heap")
assert(#filtered.frames == 1 and filtered.frames[1].frameId == 2 and filtered.frames[1].absoluteId == 100)
assert(filtered.counters[1].path == "memory/heap")
assert(disposed == 2 and iterDisposed == 3)
element.EnterTimestampFrameLocal = function()
	return currentFrame * 100
end
local recursive = analyze(lib, buffer.create(1), 2, 5)
assert(recursive.frames[1].scopes[1].inclusiveMs == 15, "Inclusive call duration remains available")
assert(recursive.frames[1].scopes[1].wallMs == 10, "Recursive instances of one timer must not inflate occupied frame time")
print("Renium MicroProfiler analysis workloads passed")
