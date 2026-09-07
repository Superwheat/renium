local Performance = require("../BridgePerformance.module")
local clock = 0
local callbacks = {}
local delays = {}
local tracking = false
local categoryReads = 0
local id = 0
local stats = setmetatable({
	HeartbeatTime = 0.001, PhysicsStepTime = 0.002, FrameTime = 0.016,
	RenderCPUFrameTime = 0.003, RenderGPUFrameTime = 0.004,
	GetTotalMemoryUsageMb = function() return 64 end,
	GetMemoryUsageMbForTag = function(_, tag)
		categoryReads += 1
		assert(tag.Name == "LuaHeap")
		return 2
	end,
}, { __index = function(_, name)
	if name == "MemoryTrackingEnabled" then return tracking end
	if name == "UI3DTriangleCount" then error("unsupported on this engine") end
	return 0
end })
local service = { Heartbeat = { Connect = function(_, callback)
	callbacks[callback] = true
	return { Disconnect = function() callbacks[callback] = nil end }
end } }
local api = Performance.create({
	stats = stats, runService = service, runtimeId = "client-one", client = true,
	studioVersion = "test-engine", bridgeVersion = "test-renium", timestamp = function() return "test-time" end,
	memoryTags = { { Name = "LuaHeap" } },
	clock = function() return clock end,
	newId = function() id += 1 return tostring(id) end,
	delay = function(_, callback) delays[#delays + 1] = callback return callback end,
	cancel = function(_) end,
})
local snapshot = api.snapshot()
assert(snapshot.timingsMs.PhysicsStepTime == 2)
assert(snapshot.metadata.studioVersion == "test-engine" and snapshot.metadata.capturedAt == "test-time")
assert(snapshot.memory.totalMb == 64 and not snapshot.memory.categoriesAvailable)
assert(categoryReads == 0 and snapshot.unavailable.UI3DTriangleCount ~= nil)
tracking = true
assert(api.snapshot().memory.categoriesMb.LuaHeap == 2 and categoryReads == 1)
for _, seconds in { 0 / 0, math.huge, -1, 0, 121, "10" } do
	assert(not pcall(api.start, { seconds = seconds }))
	assert(next(callbacks) == nil)
end
local current = api.start({ seconds = 120 })
assert(not pcall(api.start, { seconds = 1 }), "starting twice must not replace the active recording")
for _ = 1, 2501 do
	clock += 1 / 60
	for callback in callbacks do callback(1 / 60) end
end
assert(not pcall(api.stop, { captureId = "wrong-runtime" }))
local stopped = api.stop({ captureId = current.captureId })
assert(stopped.frameCount == 2501 and next(callbacks) == nil)
local compact = api.read({ captureId = current.captureId })
assert(compact.frames == nil and compact.counters == nil and #compact.slowestFrames == 5)
assert(math.abs(compact.heartbeatMs.p95 - 1000 / 60) < 0.00001)
local total = 0
for page = 1, 3 do
	local result = api.read({ captureId = current.captureId, page = page })
	total += #result.frames
	assert(result.frames[1][2] == (1 / 60) * 1000)
	assert((result.counters ~= nil) == (page == 1))
	assert(result.heartbeatMs == nil and result.slowestFrames == nil, "raw pages must not recompute a whole-capture summary")
end
assert(total == 2501, "paging must preserve every frame exactly once")
for _, page in { 0, -1, 1.5, 61, 0 / 0, math.huge, "1" } do
	assert(not pcall(api.read, { captureId = current.captureId, page = page }))
end
local nextCapture = api.start({ seconds = 1 })
delays[1]()
assert(api.read({ captureId = nextCapture.captureId }).state == "recording", "old deadline must not stop a new capture")
delays[2]()
assert(api.read({ captureId = nextCapture.captureId }).state == "complete", "deadline must stop a paused recording")
api.cleanup()
assert(next(callbacks) == nil and not pcall(api.read, { captureId = nextCapture.captureId }))
print("Renium performance capture workloads passed")
