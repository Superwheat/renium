local BridgePerformance = {}

-- Engine timings overlap and can describe different pipeline frames. They are
-- measurements, not additive slices of Heartbeat or proof of a lag cause.
local FRAME_FIELDS = { "HeartbeatTime", "PhysicsStepTime" }
local RENDER_FIELDS = { "FrameTime", "RenderCPUFrameTime", "RenderGPUFrameTime" }
local COUNTER_FIELDS = {
	"InstanceCount", "PrimitivesCount", "MovingPrimitivesCount", "ContactsCount",
	"DataReceiveKbps", "DataSendKbps", "PhysicsReceiveKbps", "PhysicsSendKbps",
}
local DRAW_FIELDS = {
	"SceneDrawcallCount", "SceneTriangleCount", "ShadowsDrawcallCount", "ShadowsTriangleCount",
	"UI2DDrawcallCount", "UI2DTriangleCount", "UI3DDrawcallCount", "UI3DTriangleCount",
}
local MAX_FRAMES = 60000
local PAGE_SIZE = 1000

function BridgePerformance.create(context)
	local stats = context.stats
	local runService = context.runService
	local clock = context.clock or os.clock
	local capture = nil
	local api = {}
	local function metadata()
		return { studioVersion = context.studioVersion, bridgeVersion = context.bridgeVersion,
			capturedAt = context.timestamp(), runtimeId = context.runtimeId }
	end

	local function selectFields(fields, unavailable)
		local available = {}
		for _, name in fields do
			-- Stats members vary with the installed engine and runtime role.
			local ok, value = pcall(function()
				return stats[name]
			end)
			if ok and type(value) == "number" then
				available[#available + 1] = name
			else
				unavailable[name] = if ok then "not numeric" else "unavailable in this engine/runtime"
			end
		end
		return available
	end

	local function fields()
		local unavailable = {}
		local frames = table.clone(FRAME_FIELDS)
		local counters = table.clone(COUNTER_FIELDS)
		if context.client then
			for _, name in RENDER_FIELDS do
				frames[#frames + 1] = name
			end
			for _, name in DRAW_FIELDS do
				counters[#counters + 1] = name
			end
		else
			for _, name in RENDER_FIELDS do
				unavailable[name] = "client measurement; select a play client"
			end
			for _, name in DRAW_FIELDS do
				unavailable[name] = "client measurement; select a play client"
			end
		end
		return selectFields(frames, unavailable), selectFields(counters, unavailable), unavailable
	end

	local function memory()
		local result = { totalMb = stats:GetTotalMemoryUsageMb(), categoriesAvailable = stats.MemoryTrackingEnabled }
		if result.categoriesAvailable then
			result.categoriesMb = {}
			for _, tag in context.memoryTags do
				result.categoriesMb[tag.Name] = stats:GetMemoryUsageMbForTag(tag)
			end
		end
		return result
	end

	local function readFields(names, scale)
		local result = {}
		for _, name in names do
			result[name] = stats[name] * scale
		end
		return result
	end

	function api.snapshot()
		local frames, counters, unavailable = fields()
		return {
			ok = true, runtimeId = context.runtimeId, client = context.client,
			metadata = metadata(),
			timingsMs = readFields(frames, 1000), counters = readFields(counters, 1),
			memory = memory(), unavailable = unavailable,
			networkRateUnit = "kilobytes/second",
		}
	end

	function api.micro()
		-- Take one immutable snapshot without yielding between the size and copy.
		-- Parsing and aggregation belong in the host, never the Studio frame loop.
		local service = context.microProfiler()
		local size = service:GetDataSize(0)
		assert(size > 0 and size <= 128 * 1024 * 1024, "MicroProfiler capture is empty or exceeds 128 MiB")
		local data = buffer.create(size)
		local copied = service:GetDataInRange(0, 0, size, data, 0)
		assert(copied == size, "MicroProfiler snapshot was incomplete; no capture was returned")
		return { ok = true, runtimeId = context.runtimeId, bytes = size, data = context.encodeBuffer(data), format = "gprx", metadata = metadata() }
	end

	local function summary(current)
		return {
			ok = true, captureId = current.id, runtimeId = context.runtimeId,
			state = if current.active then "recording" else "complete",
			elapsedMs = ((current.finished or clock()) - current.started) * 1000,
			frameCount = #current.frames, counterCount = #current.counters,
			overheadMs = current.overheadMs, stopReason = current.stopReason,
			columns = current.columns, counterColumns = current.counterColumns,
			unavailable = current.unavailable, pageSize = PAGE_SIZE,
			metadata = current.metadata,
		}
	end

	local function stop(current, reason)
		if current.active then
			current.active = false
			current.finished = clock()
			current.stopReason = reason
			current.connection:Disconnect()
			if current.timer then
				context.cancel(current.timer)
				current.timer = nil
			end
		end
		return summary(current)
	end

	function api.start(params)
		local seconds = params.seconds or 10
		assert(type(seconds) == "number" and seconds >= 0.1 and seconds <= 120, "Capture duration must be 0.1–120 seconds")
		assert(not capture or not capture.active, "A performance capture is already recording in this runtime")
		local frameFields, counterFields, unavailable = fields()
		local columns = { "elapsedMs", "heartbeatIntervalMs" }
		for _, name in frameFields do
			columns[#columns + 1] = name .. "Ms"
		end
		local counterColumns = { "elapsedMs", "totalMemoryMb" }
		for _, name in counterFields do
			counterColumns[#counterColumns + 1] = name
		end
		local current = {
			id = context.newId(), active = true, started = clock(), frames = {}, counters = {},
			columns = columns, counterColumns = counterColumns, unavailable = unavailable,
			overheadMs = 0, memoryStart = memory(), nextCounter = 0,
			metadata = metadata(),
		}
		current.connection = runService.Heartbeat:Connect(function(delta)
			local started = clock()
			local elapsed = started - current.started
			local row = { elapsed * 1000, delta * 1000 }
			for _, name in frameFields do
				row[#row + 1] = stats[name] * 1000
			end
			current.frames[#current.frames + 1] = row
			if elapsed >= current.nextCounter then
				local counter = { elapsed * 1000, stats:GetTotalMemoryUsageMb() }
				for _, name in counterFields do
					counter[#counter + 1] = stats[name]
				end
				current.counters[#current.counters + 1] = counter
				current.nextCounter = elapsed + 0.2
			end
			current.overheadMs += (clock() - started) * 1000
			if #current.frames >= MAX_FRAMES then
				stop(current, "frame-limit")
			elseif elapsed >= seconds then
				stop(current, "duration")
			end
		end)
		capture = current
		-- Also bounds captures while Play is paused (no Heartbeat).
		current.timer = context.delay(seconds, function()
			current.timer = nil
			if capture == current and current.active then
				stop(current, "duration")
			end
		end)
		return summary(current)
	end

	local function selected(params)
		assert(capture and params.captureId == capture.id, "Unknown performance capture; it may belong to another runtime or have been replaced")
		return capture
	end

	function api.stop(params)
		return stop(selected(params), "requested")
	end

	function api.read(params)
		local current = selected(params)
		local result = summary(current)
		if params.page ~= nil then
			local page = params.page
			assert(type(page) == "number" and page >= 1 and page <= MAX_FRAMES / PAGE_SIZE and page % 1 == 0, "Page must be an integer from 1 to 60")
			local start = (page - 1) * PAGE_SIZE + 1
			local finish = math.min(start + PAGE_SIZE - 1, #current.frames)
			result.frames = table.create(math.max(0, finish - start + 1))
			for index = start, finish do
				result.frames[#result.frames + 1] = current.frames[index]
			end
			result.page = page
			result.nextPage = if finish < #current.frames then page + 1 else nil
			if page == 1 then
				result.counters = table.clone(current.counters)
				result.memoryStart = current.memoryStart
			end
			return result
		end
		local intervals = table.create(#current.frames)
		local total = 0
		local slowest = {}
		for index, row in current.frames do
			local interval = row[2]
			intervals[index] = interval
			total += interval
			local position = #slowest + 1
			while position > 1 and interval > slowest[position - 1].intervalMs do
				position -= 1
			end
			if position <= 5 then
				table.insert(slowest, position, { frame = index, elapsedMs = row[1], intervalMs = interval })
				if #slowest > 5 then
					table.remove(slowest)
				end
			end
		end
		table.sort(intervals)
		if #intervals > 0 then
			result.heartbeatMs = {
				mean = total / #intervals, p50 = intervals[math.ceil(#intervals * 0.5)],
				p95 = intervals[math.ceil(#intervals * 0.95)], max = intervals[#intervals],
			}
		end
		result.slowestFrames = slowest
		return result
	end

	function api.cleanup()
		if capture then
			stop(capture, "runtime-ended")
		end
		capture = nil
	end

	return api
end

return BridgePerformance
