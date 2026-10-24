local Gui = {}

-- Roblox's installed LoadingScript owns the default experience-loading UI.
-- The native bridge holds its replication completion until travel finishes.
function Gui.create(runtimeId)
	local relay, pending, place, attempt
	local function publish(show)
		if not relay then
			relay = Instance.new("BindableFunction")
			relay.Name = "__ReniumTeleportGui_" .. runtimeId
			relay.Parent = game:GetService("CoreGui")
		end
		relay.OnInvoke = function()
			return game:GetService("ScriptContext"), game:GetService("CoreGui"), game:GetService("ReplicatedFirst")
		end
		pending = { path = { "CoreGui", relay.Name }, show = show }
	end
	local function dismiss()
		if relay then
			relay:Destroy()
			relay = nil
		end
		pending, place, attempt = nil, nil, nil
	end
	local function finish()
		if place then
			publish(false)
			attempt = nil
		end
	end
	return {
		show = function(params)
			if attempt ~= params.id then
				place, attempt = params.placeId, params.id
				publish(true)
			end
		end,
		fail = finish,
		arrive = finish,
		dismiss = dismiss,
		get = function() return nil end,
		native = function()
			if pending then
				local notification = pending
				pending = nil
				return notification
			end
			return nil
		end,
	}
end

return Gui
