local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local Players = game:GetService("Players")

local Teleport = {}

-- Compatibility entry point when native request interception is unavailable.
function Teleport.Teleport(placeId, players, options)
	assert(RunService:IsStudio() and RunService:IsServer() and RunService:IsRunning(), "ReniumTeleport.Teleport requires a Studio Play server")
	return script:WaitForChild("Request"):Invoke(placeId, players, options)
end

function Teleport.GetContext()
	assert(RunService:IsStudio(), "ReniumTeleport is a Studio-only compatibility shim")
	return HttpService:JSONDecode(script:WaitForChild("Context").Value)
end

function Teleport.GetJoinData(player)
	local context = Teleport.GetContext()
	local original = player:GetJoinData()
	if context.mode ~= "emulated" then
		return original
	end
	local entry = context.data[player.Name]
	return {
		SourcePlaceId = context.sourcePlaceId,
		SourceGameId = context.gameId,
		TeleportData = if entry then entry.teleportData else nil,
		Members = original.Members,
		Emulated = true,
	}
end

function Teleport.GetLocalPlayerTeleportData()
	assert(RunService:IsClient(), "GetLocalPlayerTeleportData requires a client")
	return Teleport.GetJoinData(Players.LocalPlayer).TeleportData
end

function Teleport.GetArrivingTeleportGui()
	assert(RunService:IsClient(), "GetArrivingTeleportGui requires a client")
	local arriving = script:FindFirstChild("ArrivingGui")
	return if arriving then arriving.Value else nil
end

return Teleport
