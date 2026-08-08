local BridgeSessionLock = {}

local LOCK_NAME = "__Renium_SessionLock"
local LOCK_MARKER = "ReniumSessionLock"
local HEARTBEAT_SECONDS = 5
local EXPIRY_SECONDS = 15
local SETTLE_SECONDS = 1.5
local SETTLE_TIMEOUT_SECONDS = 6

function BridgeSessionLock.create(runtimeId: string, ownershipLost: (() -> ())?)
	local ServerStorage = game:GetService("ServerStorage")
	local StudioService = game:GetService("StudioService")
	local RunService = game:GetService("RunService")
	local userId = StudioService:GetUserId()
	local project = if game.GameId > 0
		then `{game.GameId}:{game.PlaceId}`
		elseif game.PlaceId > 0 then tostring(game.PlaceId)
		else game.Name
	local heartbeatToken = 0
	local ownershipGeneration = 0
	local active = false
	local watchedLocks: { [Folder]: { RBXScriptConnection } } = {}

	local function isSessionLock(value: Instance): boolean
		return value:IsA("Folder") and value.Name == LOCK_NAME and value:GetAttribute(LOCK_MARKER) == true
	end

	local function locks(): { Folder }
		local values = {}
		for _, value in ServerStorage:GetChildren() do
			if isSessionLock(value) then
				values[#values + 1] = value :: Folder
			end
		end
		return values
	end

	local function owns(lock: Folder?): boolean
		return lock ~= nil and lock:GetAttribute("Runtime") == runtimeId
	end

	local function expired(lock: Folder, now: number): boolean
		local heartbeat = tonumber(lock:GetAttribute("Heartbeat")) or 0
		return heartbeat <= 0 or now - heartbeat > EXPIRY_SECONDS
	end

	local function elected(now: number): Folder?
		local winner = nil
		local winnerRuntime = nil
		for _, lock in locks() do
			if not expired(lock, now) or active and owns(lock) then
				local lockRuntime = tostring(lock:GetAttribute("Runtime") or "")
				if lockRuntime ~= "" and (winnerRuntime == nil or lockRuntime < winnerRuntime) then
					winner = lock
					winnerRuntime = lockRuntime
				end
			end
		end
		return winner
	end

	local function candidateSignature(now: number): string
		local runtimes = {}
		for _, lock in locks() do
			if not expired(lock, now) then
				runtimes[#runtimes + 1] = tostring(lock:GetAttribute("Runtime") or "")
			end
		end
		table.sort(runtimes)
		return table.concat(runtimes, "\0")
	end

	local function owned(): Folder?
		for _, lock in locks() do
			if owns(lock) then
				return lock
			end
		end
		return nil
	end

	local function describe(lock: Folder?): { [string]: any }
		if lock == nil then
			return {
				active = false,
				owned = false,
			}
		end
		local heartbeat = tonumber(lock:GetAttribute("Heartbeat")) or 0
		local ageSeconds = math.max(0, os.time() - heartbeat)
		return {
			active = not expired(lock, os.time()),
			owned = owns(lock),
			project = tostring(lock:GetAttribute("Project") or ""),
			runtime = tostring(lock:GetAttribute("Runtime") or ""),
			userId = tonumber(lock:GetAttribute("UserId")) or 0,
			heartbeat = heartbeat,
			ageSeconds = ageSeconds,
			retryAfterSeconds = math.max(1, EXPIRY_SECONDS - ageSeconds + 1),
		}
	end

	local function write(lock: Folder)
		lock:SetAttribute(LOCK_MARKER, true)
		lock:SetAttribute("Project", project)
		lock:SetAttribute("Runtime", runtimeId)
		lock:SetAttribute("UserId", userId)
		lock:SetAttribute("Heartbeat", os.time())
	end

	local function removeOwnedLocks()
		for _, lock in locks() do
			if owns(lock) then
				lock:Destroy()
			end
		end
	end

	local function loseOwnership()
		if not active then
			return
		end
		active = false
		heartbeatToken += 1
		ownershipGeneration += 1
		removeOwnedLocks()
		if ownershipLost ~= nil then
			ownershipLost()
		end
	end

	local function verifyOwnership()
		if not active then
			return
		end
		task.defer(function()
			if not active then
				return
			end
			local lock = owned()
			if lock == nil or elected(os.time()) ~= lock then
				loseOwnership()
			end
		end)
	end

	local function watchLock(lock: Folder)
		if watchedLocks[lock] ~= nil then
			return
		end
		local connections = {
			lock:GetAttributeChangedSignal("Runtime"):Connect(verifyOwnership),
			lock:GetAttributeChangedSignal("Heartbeat"):Connect(verifyOwnership),
			lock:GetAttributeChangedSignal(LOCK_MARKER):Connect(verifyOwnership),
			lock:GetPropertyChangedSignal("Name"):Connect(verifyOwnership),
		}
		watchedLocks[lock] = connections
		lock.Destroying:Connect(function()
			for _, connection in connections do
				connection:Disconnect()
			end
			watchedLocks[lock] = nil
			verifyOwnership()
		end)
	end

	for _, lock in locks() do
		watchLock(lock)
	end
	ServerStorage.ChildAdded:Connect(function(child)
		if isSessionLock(child) then
			watchLock(child :: Folder)
			verifyOwnership()
		end
	end)
	ServerStorage.ChildRemoved:Connect(verifyOwnership)

	local function startHeartbeat()
		heartbeatToken += 1
		local token = heartbeatToken
		task.spawn(function()
			while token == heartbeatToken do
				task.wait(HEARTBEAT_SECONDS)
				if token ~= heartbeatToken then
					return
				end
				local lock = owned()
				if lock == nil or elected(os.time()) ~= lock then
					loseOwnership()
					return
				end
				lock:SetAttribute("Heartbeat", os.time())
			end
		end)
	end

	local api = {}

	function api.inspect(): { [string]: any }
		return describe(elected(os.time()))
	end

	function api.acquire(takeover: boolean?): (boolean, { [string]: any })
		if takeover == true then
			for _, lock in locks() do
				if not owns(lock) then
					lock:Destroy()
				end
			end
		end
		local lock = owned()
		if lock == nil then
			lock = Instance.new("Folder")
			lock.Name = LOCK_NAME
			lock.Archivable = false
			write(lock)
			lock.Parent = ServerStorage
			watchLock(lock)
		else
			write(lock)
		end
		local started = os.clock()
		local stableSince = started
		local signature = candidateSignature(os.time())
		while os.clock() - stableSince < SETTLE_SECONDS do
			RunService.Heartbeat:Wait()
			local winner = elected(os.time())
			if winner ~= lock then
				removeOwnedLocks()
				return false, describe(winner)
			end
			local currentSignature = candidateSignature(os.time())
			if currentSignature ~= signature then
				signature = currentSignature
				stableSince = os.clock()
			end
			if os.clock() - started >= SETTLE_TIMEOUT_SECONDS then
				removeOwnedLocks()
				return false, describe(winner)
			end
		end
		for _ = 1, 2 do
			RunService.Heartbeat:Wait()
			if elected(os.time()) ~= lock or candidateSignature(os.time()) ~= signature then
				removeOwnedLocks()
				return false, describe(elected(os.time()))
			end
		end
		active = true
		ownershipGeneration += 1
		startHeartbeat()
		return true, describe(lock)
	end

	function api.release()
		active = false
		heartbeatToken += 1
		ownershipGeneration += 1
		removeOwnedLocks()
	end

	function api.owns(): boolean
		local lock = owned()
		return active and lock ~= nil and elected(os.time()) == lock
	end

	function api.capture(): number?
		return if api.owns() then ownershipGeneration else nil
	end

	function api.validate(generation: number?): boolean
		return generation ~= nil and generation == ownershipGeneration and api.owns()
	end

	function api.isLockInstance(instance: Instance): boolean
		local value = instance
		while value ~= ServerStorage do
			if isSessionLock(value) then
				return true
			end
			local parent = value.Parent
			if parent == nil then
				return false
			end
			value = parent
		end
		return false
	end

	return api
end

return BridgeSessionLock
