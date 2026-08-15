local BridgeSessionLock = {}

local LOCK_NAME = "__Renium_SessionLock"
local LOCK_MARKER = "ReniumSessionLock"

function BridgeSessionLock.create(runtimeId: string, ownershipLost: () -> (), useTeamCreateLock: boolean)
	local Players = game:GetService("Players")
	local ServerStorage = game:GetService("ServerStorage")
	local ownershipGeneration = 0
	local active = false
	local function teamCreateActive(): boolean
		return useTeamCreateLock and #Players:GetChildren() > 0
	end

	local function isSessionLock(value: Instance): boolean
		return value.Name == LOCK_NAME
			and (value:IsA("ObjectValue") or value:IsA("Folder") and value:GetAttribute(LOCK_MARKER) == true)
	end

	local function locks(): { Instance }
		local values = {}
		for _, value in ServerStorage:GetChildren() do
			if isSessionLock(value) then
				values[#values + 1] = value
			end
		end
		return values
	end

	local function runtime(lock: Instance): string
		return tostring(lock:GetAttribute("Runtime") or "")
	end

	local function removeOwnedLocks()
		for _, lock in locks() do
			if runtime(lock) == runtimeId then
				lock:Destroy()
			end
		end
	end

	local function current(): Instance?
		local teamCreate = teamCreateActive()
		local winner = nil
		local winnerRuntime = nil
		for _, lock in ServerStorage:GetChildren() do
			if isSessionLock(lock) then
				local stale = not teamCreate or lock:IsA("Folder")
				if not stale then
					local owner = (lock :: ObjectValue).Value
					stale = owner == nil or owner.Parent ~= Players
				end
				if stale then
					lock:Destroy()
				else
					local lockRuntime = runtime(lock)
					if lockRuntime ~= "" and (winnerRuntime == nil or lockRuntime < winnerRuntime) then
						winner = lock
						winnerRuntime = lockRuntime
					end
				end
			end
		end
		return winner
	end

	local function owned(): Instance?
		for _, lock in locks() do
			if runtime(lock) == runtimeId then
				return lock
			end
		end
		return nil
	end

	local function describe(lock: Instance?): { [string]: any }
		if lock == nil then
			return {
				active = active and not teamCreateActive(),
				owned = active and not teamCreateActive(),
			}
		end
		local userId = tonumber(lock:GetAttribute("UserId")) or 0
		local owner = if lock:IsA("ObjectValue") then lock.Value else nil
		if owner and owner:IsA("Player") then
			userId = owner.UserId
		end
		return {
			active = true,
			owned = runtime(lock) == runtimeId,
			runtime = runtime(lock),
			userId = userId,
		}
	end

	local api = {}

	function api.inspect(): { [string]: any }
		return describe(current())
	end

	function api.acquire(takeover: boolean?): (boolean, { [string]: any })
		local winner = current()
		if not teamCreateActive() then
			active = true
			ownershipGeneration += 1
			return true, describe(nil)
		end

		local owner = Players.LocalPlayer
		if owner == nil then
			return false, describe(current())
		end
		if takeover then
			for _, lock in locks() do
				if runtime(lock) ~= runtimeId then
					lock:Destroy()
				end
			end
			winner = current()
		end
		if winner and runtime(winner) ~= runtimeId then
			return false, describe(winner)
		end

		local lock = owned()
		if lock == nil or not lock:IsA("ObjectValue") then
			if lock then
				lock:Destroy()
			end
			lock = Instance.new("ObjectValue")
			lock.Name = LOCK_NAME
			lock.Archivable = false
			lock:SetAttribute(LOCK_MARKER, true)
			lock:SetAttribute("Runtime", runtimeId)
			lock:SetAttribute("UserId", owner.UserId)
			lock.Value = owner
			lock.Parent = ServerStorage
		else
			lock.Value = owner
		end

		winner = current()
		if winner ~= lock then
			removeOwnedLocks()
			return false, describe(winner)
		end
		active = true
		ownershipGeneration += 1
		return true, describe(lock)
	end

	function api.release()
		active = false
		ownershipGeneration += 1
		removeOwnedLocks()
	end

	function api.owns(): boolean
		if not active then
			return false
		end
		if not teamCreateActive() then
			return true
		end
		local lock = owned()
		return lock ~= nil and current() == lock
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

	local ownershipCheckPending = false
	local function verifyOwnership()
		if not active or ownershipCheckPending then
			return
		end
		ownershipCheckPending = true
		task.defer(function()
			ownershipCheckPending = false
			if active and not api.owns() then
				active = false
				ownershipGeneration += 1
				removeOwnedLocks()
				ownershipLost()
			end
		end)
	end

	current()
	ServerStorage.ChildAdded:Connect(function(child)
		if isSessionLock(child) then
			verifyOwnership()
		end
	end)
	ServerStorage.ChildRemoved:Connect(function(child)
		if isSessionLock(child) then
			verifyOwnership()
		end
	end)

	return api
end

return BridgeSessionLock
