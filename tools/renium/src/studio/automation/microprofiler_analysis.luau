-- Snapshot-only analysis. Scopes are inclusive work per thread, never summed
-- into a claimed frame/critical-path time. The original capture keeps all data.
return function(lib, bytes, selectedFrame, top, scopeMask, threadMask, counterMask)
	local session = lib.Session.OpenFromBuffer(bytes)
	assert(session and session:IsValid(), "Invalid or unsupported MicroProfiler capture")
	local first, last = session:GetFrameIdMin(), session:GetFrameIdMax()
	assert(last >= first and first > 0, "Capture contains no frames; enable MicroProfiler capture before recording")
	assert(not selectedFrame or (selectedFrame >= first and selectedFrame <= last), `Frame is outside capture range {first}–{last}`)
	local scale = session:FetchGlobalDesc()
	local descriptors, frames, ranked, durations, segments = {}, {}, {}, {}, {}
	local previousAbsolute = nil
	for id = first, last do
		local desc = session:FetchFrameDesc(id)
		descriptors[id] = desc
		if previousAbsolute == nil or desc.FrameAbsoluteId ~= previousAbsolute + 1 then
			segments[#segments + 1] = { firstFrame = id, firstAbsoluteId = desc.FrameAbsoluteId }
			table.clear(ranked)
			table.clear(durations)
		end
		previousAbsolute = desc.FrameAbsoluteId
		segments[#segments].lastFrame = id
		segments[#segments].lastAbsoluteId = desc.FrameAbsoluteId
		local row = {
			frameId = id, absoluteId = desc.FrameAbsoluteId,
			cpuMs = (desc.TickEndCpu - desc.TickStartCpu) * scale.TickToMsCpu,
			incomplete = desc.IsIncomplete, paused = desc.IsPaused,
		}
		frames[#frames + 1] = row
		if row.cpuMs >= 0 and not row.incomplete and not row.paused then
			ranked[#ranked + 1] = row
			durations[#durations + 1] = row.cpuMs
		end
	end
	table.sort(ranked, function(a, b) return a.cpuMs > b.cpuMs end)
	table.sort(durations)
	local chosen = if selectedFrame then { frames[selectedFrame - first + 1] } else ranked
	local details = {}
	local timerIds = if scopeMask then session:FindTimerIds(scopeMask, false) else nil
	local threadIds = if threadMask then session:FindThreadIds(threadMask, false) else nil
	assert(not timerIds or #timerIds > 0, "No scopes match the requested filter")
	assert(not threadIds or #threadIds > 0, "No threads match the requested filter")
	for index = 1, math.min(if selectedFrame then 1 else 5, #chosen) do
		local frame = chosen[index]
		local desc = descriptors[frame.frameId]
		local iter = session:CreateLogIterator()
		iter:Configure({ SkipEvents = true, EmitThreadFrameEnd = true, TimerIds = timerIds, ThreadIds = threadIds })
		iter:RewindTo(frame.frameId, frame.frameId)
		local state = iter:GetState()
		local scopes, threads, intervals = {}, {}, {}
		local function add(timerId, threadId, beginTick, endTick)
			local thread = threads[threadId]
			if not thread then
				thread = session:FetchThreadDesc(threadId)
				assert(thread, `Capture omitted thread {threadId} metadata in frame {frame.frameId}`)
				threads[threadId] = thread
			end
			local start = if thread.IsGpu then desc.TickStartGpu else desc.TickStartCpu
			local finish = if thread.IsGpu then desc.TickEndGpu else desc.TickEndCpu
			if finish < start then return end
			local elapsed = (math.min(endTick, finish) - math.max(beginTick, start))
				* (if thread.IsGpu then scale.TickToMsGpu else scale.TickToMsCpu)
			if elapsed <= 0 then return end
			local key = `{threadId}:{timerId}`
			local item = scopes[key]
			if not item then
				local timer = session:FetchTimerDesc(timerId)
				local group = session:FetchGroupDesc(timer.GroupId)
				item = { timerId = timerId, name = timer.TimerName, group = group.GroupName,
					threadId = threadId, thread = thread.ThreadName, gpu = thread.IsGpu, inclusiveMs = 0, spans = 0 }
				scopes[key] = item
				intervals[key] = {}
			end
			item.inclusiveMs += elapsed
			item.spans += 1
			local ranges = intervals[key]
			ranges[#ranges + 1] = { math.max(beginTick, start), math.min(endTick, finish) }
		end
		local overflow = false
		while iter:Step() do
			if state:ThreadStackIsOverflowed() then overflow = true end
			if state:IsThreadFrameEnd() then
				for depth = state:ThreadStackDepth() - 1, 0, -1 do
					local element = iter:GetCurrentThreadStackElement(depth)
					if element then add(element:EnterTimerId(), state:ThreadId(), element:EnterTimestampFrameLocal(), state:Timestamp()) end
				end
			elseif state:IsExit() and not state:ThreadStackWasOverflowed() then
				local beginTick
				if state:ThreadStackIsUnderflowed() then
					beginTick = if state:IsGpuThread() then desc.TickStartGpu else desc.TickStartCpu
				else
					beginTick = iter:GetCurrentThreadStackElement(state:ThreadStackDepth()):EnterTimestampFrameLocal()
				end
				add(state:TimerId(), state:ThreadId(), beginTick, state:Timestamp())
			end
		end
		iter:Dispose()
		local ordered, idle = {}, {}
		for key, item in scopes do
			-- Recursive scopes can repeat the same timer on the same thread.
			-- Keep inclusive call time, but rank the occupied wall interval once.
			local ranges = intervals[key]
			table.sort(ranges, function(a, b)
				return a[1] < b[1]
			end)
			local covered, finish = 0, -math.huge
			for _, range in ranges do
				covered += math.max(0, range[2] - math.max(finish, range[1]))
				finish = math.max(finish, range[2])
			end
			item.wallMs = covered * (if item.gpu then scale.TickToMsGpu else scale.TickToMsCpu)
			local target = if item.name == "Sleep" and not scopeMask then idle else ordered
			target[#target + 1] = item
		end
		table.sort(ordered, function(a, b)
			return a.wallMs > b.wallMs
		end)
		local count = #ordered
		while #ordered > top do table.remove(ordered) end
		details[#details + 1] = { frameId = frame.frameId, absoluteId = frame.absoluteId,
			cpuMs = frame.cpuMs, scopeCount = count, scopes = ordered, sleepingThreads = #idle, stackOverflow = overflow }
	end
	local counters = {}
	local names = {}
	local function counterPath(id)
		if id == 0 then return "" end
		if names[id] then return names[id] end
		local desc = session:FetchCounterDesc(id)
		local parent = counterPath(desc.Parent)
		local name = if parent == "" then desc.CounterName else `{parent}/{desc.CounterName}`
		names[id] = name
		return name
	end
	local counterIds = if counterMask then session:FindCounterIds(counterMask, false) else session:FetchCounterIds()
	for _, id in counterIds do
		local sample = session:FetchLastCounterSample(id)
		if sample and sample.Value ~= 0 then
			counters[#counters + 1] = { id = id, path = counterPath(id), value = sample.Value, frameId = sample.FrameId }
		end
	end
	table.sort(counters, function(a, b) return a.path < b.path end)
	local count = #counters
	while #counters > top do table.remove(counters) end
	session:Dispose()
	local total = 0
	for _, value in durations do total += value end
	return {
		frameCount = #frames, firstFrame = first, lastFrame = last,
		segments = segments, statisticsSegment = #segments, measuredFrameCount = #durations,
		cpuMs = if #durations > 0 then { mean = total / #durations, p50 = durations[math.ceil(#durations * 0.5)],
			p95 = durations[math.ceil(#durations * 0.95)], max = durations[#durations] } else nil,
		frames = details, counterCount = count, counters = counters,
		units = { scopes = "wallMs: interval union per thread/timer; inclusiveMs: sum of calls including recursion; different scopes still overlap", counters = "engine-native units" },
		limitations = "Scope instance labels and event metadata are not exposed by LibMP; frame GPU bounds alone do not establish GPU-bound work.",
	}
end
