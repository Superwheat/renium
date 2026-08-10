local BridgeParallel = {}

local SERIALIZATION_BURST_BUDGET_SECONDS = 1 / 240
local SERIALIZATION_BURST_CHECK_INTERVAL = 64
local MAX_PARALLEL_CHUNK_WORKERS = 4
local PARALLEL_TARGET_ITEMS_PER_WORKER = 256

function BridgeParallel.makeBurstYielder(checkInterval, budgetSeconds)
	local interval = math.max(1, checkInterval or SERIALIZATION_BURST_CHECK_INTERVAL)
	local budget = budgetSeconds or SERIALIZATION_BURST_BUDGET_SECONDS
	local untilCheck = interval
	local burstStarted = os.clock()
	return function()
		untilCheck -= 1
		if untilCheck > 0 then
			return
		end
		untilCheck = interval
		if os.clock() - burstStarted >= budget then
			task.wait()
			burstStarted = os.clock()
		end
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
