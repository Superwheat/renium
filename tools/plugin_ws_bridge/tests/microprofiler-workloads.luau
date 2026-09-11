local MicroProfiler = require("../BridgeMicroProfiler.module")
local calls = {}
local protocol = 65536
local failAck = false
local failCommand = nil
local failSnapshot = false
local service = { GetDataSize = function(_, slot)
	assert(not failSnapshot, "snapshot initialization failed")
	assert(slot == 0 and calls[#calls][1] == 20 and calls[#calls][2] == 1, "Initialize snapshot descriptors after enabling collection")
	return 1
end, ProcessCommand = function(_, source, _, length, response)
	if length == 0 then
		for i = 0, 3 do buffer.writeu32(response, i * 4, protocol) end
		return 20
	end
	local command = buffer.readu16(source, 8)
	assert(command == 10 or command == 20 or command == 40)
	assert(buffer.readu32(source, 4) == 0x46423041)
	assert(buffer.readu16(source, 10) == 1)
	local width = if command == 40 then 4 else 1
	assert(length == 32 + width and buffer.readu32(source, 24) == width)
	calls[#calls + 1] = { command, if width == 4 then buffer.readu32(source, 32) else buffer.readu8(source, 32) }
	buffer.copy(response, 0, source, 0, 32)
	buffer.writeu32(response, 16, 0x00010401)
	buffer.writeu32(response, 24, 1)
	buffer.writeu32(response, 28, 1)
	local reject = failAck or (failCommand ~= nil and failCommand[1] == command and failCommand[2] == calls[#calls][2])
	if reject and failCommand then failCommand = nil end
	buffer.writeu8(response, 32, if reject then 0 else 1)
	return 33
end }
local afterPause = nil
local completedFrames = 0
local api = MicroProfiler.create(function() return service end, function()
	assert(calls[#calls][1] == 20 and calls[#calls][2] == 0, "Finish the current frame only after pausing collection")
	completedFrames += 1
	if afterPause then afterPause() end
end)
for _, value in { 0, 257, -1, 1.5, math.huge, 0 / 0, "256" } do
	assert(not pcall(api.start, value) and #calls == 0)
end
api.start(256)
assert(#calls == 3 and calls[1][1] == 40 and calls[1][2] == 256 and calls[2][1] == 10 and calls[3][1] == 20)
api.stop()
assert(calls[4][1] == 20 and calls[4][2] == 0)
assert(calls[5][1] == 10 and calls[5][2] == 0, "Stop must not leave instrumentation enabled")
protocol = 2
assert(not pcall(api.start, 128) and #calls == 5, "Unknown protocol must not perform any control writes")
protocol = 65536
failAck = true
assert(not pcall(api.start, 128) and #calls == 6, "Rejected limit must not continue enabling the capture")
failAck = false
api.start(64)
local readResult = api.read(function()
	assert(calls[#calls][1] == 20 and calls[#calls][2] == 0, "Read must freeze the owned capture")
	return "snapshot"
end)
assert(readResult == "snapshot" and calls[#calls][1] == 20 and calls[#calls][2] == 1, "Read must resume the owned capture")
local readOk = pcall(api.read, function() error("copy failed") end)
assert(not readOk and calls[#calls][1] == 20 and calls[#calls][2] == 1, "A failed read must resume the owned capture")
local fresh = api.stop(function()
	assert(calls[#calls][1] == 20 and calls[#calls][2] == 0, "Snapshot bytes must be copied only after capture is paused")
	return { bytes = 123 }
end)
assert(fresh.bytes == 123 and calls[#calls][2] == 0)
api.start(64)
local ok, err = pcall(api.stop, function() error("incomplete snapshot") end)
assert(not ok and tostring(err):find("incomplete snapshot") and calls[#calls][2] == 0, "A failed snapshot must still stop collection")
for _, failure in { { 10, 1 }, { 20, 1 } } do
	failCommand = failure
	assert(not pcall(api.start, 64))
	assert(calls[#calls - 1][1] == 20 and calls[#calls - 1][2] == 0)
	assert(calls[#calls][1] == 10 and calls[#calls][2] == 0, "Failed startup must disable instrumentation")
end
failSnapshot = true
assert(not pcall(api.start, 64) and calls[#calls][1] == 10 and calls[#calls][2] == 0)
failSnapshot = false
api.start(64)
local beforeDuplicate = #calls
assert(not pcall(api.start, 64) and #calls == beforeDuplicate, "Do not replace an active capture")
failCommand = { 20, 0 }
assert(not pcall(api.stop))
assert(calls[#calls][1] == 10 and calls[#calls][2] == 0, "Failure pausing capture must still disable instrumentation")
api.cleanup()
local afterCleanup = #calls
api.cleanup()
assert(#calls == afterCleanup, "Cleanup must not touch another capture when this runtime is inactive")
assert(api.read(function() return "unowned" end) == "unowned" and #calls == afterCleanup, "An unowned read must not change collection")
api.start(64)
afterPause = function()
	assert(not pcall(api.read, function() return "overlap" end), "Concurrent copying must be rejected")
	api.cleanup()
	assert(not pcall(api.start, 64), "Do not start a new capture while an old copy is completing")
end
local cancelled = pcall(api.read, function() error("Must not copy after cleanup") end)
assert(not cancelled and calls[#calls][1] == 10 and calls[#calls][2] == 0, "Cleanup during the frame wait must not resume collection")
afterPause = nil
api.start(64)
api.stop()
assert(completedFrames >= 5, "Snapshot copies must finish the paused frame")
print("Renium MicroProfiler protocol workloads passed")
