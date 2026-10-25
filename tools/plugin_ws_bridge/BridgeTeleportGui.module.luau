local Gui = {}

-- Roblox's installed LoadingScript owns the default experience-loading UI.
-- The native bridge holds its replication completion until travel finishes.
function Gui.create(runtimeId)
	local relay, pending, place, attempt
	local destinationName = ""
	local connections, labels = {}, {}
	local core = game:GetService("CoreGui")
	local function disconnect()
		for _, connection in connections do
			connection:Disconnect()
		end
		table.clear(connections)
		table.clear(labels)
	end
	local function observe(object)
		local root = object
		while root.Parent and root.Parent ~= core do
			root = root.Parent
		end
		if root.Name ~= "RobloxLoadingGUI" then return end
		if object:IsA("TextLabel") and object.Name == "PlaceLabel" then
			local function update()
				if object.Text ~= destinationName then
					object.Text = destinationName
				end
			end
			labels[object] = update
			table.insert(connections, object:GetPropertyChangedSignal("Text"):Connect(update))
			update()
		elseif object:IsA("ImageLabel") then
			-- The universe icon stays the same; place thumbnail URLs must follow
			-- the destination. Preserve Roblox's image type, size and styling.
			local function update()
				local image = object.Image:gsub(`([?&]id=){game.PlaceId}(&)`, `%1{place}%2`)
				if image ~= object.Image then
					object.Image = image
				end
			end
			table.insert(connections, object:GetPropertyChangedSignal("Image"):Connect(update))
			update()
		end
	end
	local function publish(show)
		if not relay then
			relay = Instance.new("BindableFunction")
			relay.Name = "__ReniumTeleportGui_" .. runtimeId
			relay.Parent = game:GetService("CoreGui")
		end
		relay.OnInvoke = function()
			return game:GetService("ScriptContext"), game:GetService("CoreGui"), game:GetService("ReplicatedFirst")
		end
		pending = { path = { "CoreGui", relay.Name }, show = show, modelName = game.Name }
	end
	local function dismiss()
		disconnect()
		if relay then
			relay:Destroy()
			relay = nil
		end
		pending, place, attempt = nil, nil, nil
	end
	local function finish()
		disconnect()
		if place then
			publish(false)
			attempt = nil
		end
	end
	return {
		show = function(params)
			if attempt ~= params.id then
				disconnect()
				destinationName = params.displayName or ""
				place, attempt = params.placeId, params.id
				table.insert(connections, core.DescendantAdded:Connect(observe))
				publish(true)
			elseif params.displayName then
				destinationName = params.displayName
				for _, update in labels do
					update()
				end
			end
		end,
		fail = finish,
		arrive = finish,
		dismiss = dismiss,
		invokeNative = function()
			assert(relay and relay.Parent == core, "Teleport GUI notification ended")
			relay:GetAttribute("__ReniumNativeGuiInvoke")
		end,
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
