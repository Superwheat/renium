local BridgeParallel = {}

local MAX_PARALLEL_CHUNK_WORKERS = 4
local PARALLEL_TARGET_ITEMS_PER_WORKER = 256

local FRAME_SECONDS = 1 / 30
local MIN_SLICE_SECONDS = 0.008
local MAX_SLICE_SECONDS = 0.1
local REQUEST_SLICE_SECONDS = 0.25
local MAX_REQUEST_SLICE_SECONDS = 1

-- task.spawn runs on the same Luau scheduler: workers must still share frames
-- with Studio's built-in plugins. Check at item boundaries, keeping small jobs
-- synchronous and returning whether callers need to invalidate a local cache.
-- Without a slice of the caller's, each slice is what a frame at 30 a second
-- has left after the time Studio took for its own work during the last wait,
-- and never less than that time itself: a Studio whose frames are slow (after
-- a large import a frame takes 50 to 200 ms) would otherwise leave the work
-- 8 ms of every frame and stretch a one-second loop to half a minute, while a
-- frame of its own length more costs it nothing a person notices.
function BridgeParallel.newWorkBudget(sliceSeconds: number?, observeWait: ((number) -> ())?)
	local slice = sliceSeconds or MIN_SLICE_SECONDS
	local deadline = os.clock() + slice
	return function()
		if os.clock() < deadline then
			return false
		end
		local started = os.clock()
		task.wait()
		local resumed = os.clock()
		if observeWait then
			observeWait(started)
		end
		if sliceSeconds == nil then
			local waited = resumed - started
			slice = math.clamp(math.max(FRAME_SECONDS - waited, waited), MIN_SLICE_SECONDS, MAX_SLICE_SECONDS)
		end
		deadline = os.clock() + slice
		return true
	end
end

-- One budget for all the work of one request, which yields rarely: right
-- after a large import one wait costs Studio 0.4 s whenever it comes. The
-- first slice is the shortest; each later one lasts as long as the last wait
-- did, within those bounds, so the work never holds Studio near the 5 s at
-- which Studio reports it as not responding. Each check may name the observer
-- that its wait is charged to, and returns true when it yielded.
function BridgeParallel.newRequestBudget()
	local deadline = os.clock() + REQUEST_SLICE_SECONDS
	return function(observeWait: ((number) -> ())?)
		if os.clock() < deadline then
			return false
		end
		local started = os.clock()
		task.wait()
		local resumed = os.clock()
		if observeWait then
			observeWait(started)
		end
		deadline = os.clock() + math.clamp(resumed - started, REQUEST_SLICE_SECONDS, MAX_REQUEST_SLICE_SECONDS)
		return true
	end
end

function BridgeParallel.getParallelChunkWorkerCount(totalItems, minItems)
	if totalItems < minItems then
		return 1
	end
	return math.clamp(math.ceil(totalItems / PARALLEL_TARGET_ITEMS_PER_WORKER), 1, MAX_PARALLEL_CHUNK_WORKERS)
end

function BridgeParallel.runParallelChunks(totalItems, workerCount, job)
	if totalItems <= 0 then
		return
	end

	local clampedWorkers = math.clamp(workerCount, 1, totalItems)
	if clampedWorkers <= 1 then
		job(1, totalItems)
		return
	end

	local completedWorkers = 0
	local firstError = nil
	local finished = Instance.new("BindableEvent")

	for workerIndex = 1, clampedWorkers do
		local startIndex = math.floor(((workerIndex - 1) * totalItems) / clampedWorkers) + 1
		local endIndex = math.floor((workerIndex * totalItems) / clampedWorkers)
		task.spawn(function()
			local ok, err = pcall(job, startIndex, endIndex)
			if not ok and firstError == nil then
				firstError = err
			end
			completedWorkers += 1
			if completedWorkers == clampedWorkers then
				finished:Fire()
			end
		end)
	end

	if completedWorkers < clampedWorkers then
		finished.Event:Wait()
	end
	finished:Destroy()

	if firstError ~= nil then
		error(firstError)
	end
end

return BridgeParallel
