local Gui = {}

function Gui.create()
	local screen, arriving, title, detail, thumbnail, spinner, animation, note
	local generation = 0
	local function label(parent, text, y, size)
		local item = Instance.new("TextLabel")
		item.BackgroundTransparency = 1
		item.Size = UDim2.new(1, -48, 0, 48)
		item.Position = UDim2.new(0, 24, 0.5, y)
		item.Font = Enum.Font.GothamMedium
		item.TextSize = size
		item.TextColor3 = Color3.fromRGB(236, 239, 245)
		item.TextWrapped = true
		item.Text = text
		item.Parent = parent
		return item
	end
	local function dismiss()
		if animation then
			animation:Disconnect()
			animation = nil
		end
		if screen then
			screen:Destroy()
			screen = nil
		end
	end
	local function show(params)
		generation += 1
		if arriving then
			arriving:Destroy()
			arriving = nil
		end
		if not screen then
			screen = Instance.new("ScreenGui")
			screen.Name = "ReniumTeleportLoading"
			screen.IgnoreGuiInset = true
			screen.ResetOnSpawn = false
			screen.DisplayOrder = 2147483647
			local background = Instance.new("Frame")
			background.Size = UDim2.fromScale(1, 1)
			background.BackgroundColor3 = Color3.fromRGB(19, 22, 29)
			background.BorderSizePixel = 0
			background.Active = true
			background.Parent = screen
			thumbnail = Instance.new("ImageLabel")
			thumbnail.BackgroundColor3 = Color3.fromRGB(34, 39, 49)
			thumbnail.BorderSizePixel = 0
			thumbnail.Size = UDim2.fromOffset(160, 160)
			thumbnail.AnchorPoint = Vector2.new(0.5, 0.5)
			thumbnail.Position = UDim2.new(0.5, 0, 0.5, -76)
			thumbnail.Parent = background
			local corner = Instance.new("UICorner")
			corner.CornerRadius = UDim.new(0, 16)
			corner.Parent = thumbnail
			title = label(background, "", 12, 26)
			detail = label(background, "Loading destination…", 64, 16)
			spinner = Instance.new("Frame")
			spinner.Size = UDim2.fromOffset(28, 28)
			spinner.AnchorPoint = Vector2.new(0.5, 0.5)
			spinner.Position = UDim2.new(0.5, 0, 0.5, 140)
			spinner.BackgroundTransparency = 1
			spinner.Parent = background
			for index = 1, 8 do
				local angle = math.rad(index * 45)
				local dot = Instance.new("Frame")
				dot.Size = UDim2.fromOffset(5, 5)
				dot.AnchorPoint = Vector2.new(0.5, 0.5)
				dot.Position = UDim2.new(0.5, math.cos(angle) * 12, 0.5, math.sin(angle) * 12)
				dot.BackgroundColor3 = Color3.fromRGB(223, 230, 245)
				dot.BackgroundTransparency = index / 10
				dot.BorderSizePixel = 0
				dot.Parent = spinner
			end
			note = label(background, "", 0, 13)
			note.Position = UDim2.new(0, 24, 1, -64)
			animation = game:GetService("RunService").RenderStepped:Connect(function(delta)
				spinner.Rotation = (spinner.Rotation + delta * 240) % 360
			end)
			screen.Parent = game:GetService("Players").LocalPlayer:WaitForChild("PlayerGui")
		end
		title.Text = `Place {params.placeId}`
		local mode = if params.mode == "native" then "NATIVE PLAYTEST"
			elseif params.mode == "checking-native" then "CHECKING NATIVE PLAYTEST"
			elseif params.nativeRequested then "STUDIO EMULATION · native unavailable"
			else "STUDIO EMULATION"
		note.Text = `{mode} · Stop Play to return to your source place`
		thumbnail.Image = `rbxthumb://type=Asset&id={params.placeId}&w=420&h=420`
		detail.Text = if params.contentSource == "cache" then `Saved version {params.version} · from cache`
			elseif params.contentSource == "download" then `Saved version {params.version} · downloaded`
			else "Checking saved version and access…"
		detail.Size = UDim2.new(1, -48, 0, 48)
		spinner.Visible = true
		if not animation then
			animation = game:GetService("RunService").RenderStepped:Connect(function(delta)
				spinner.Rotation = (spinner.Rotation + delta * 240) % 360
			end)
		end
	end
	local function fail(message, stopRequired)
		if not screen then
			return
		end
		if animation then
			animation:Disconnect()
			animation = nil
		end
		spinner.Visible = false
		detail.Text = if stopRequired then `Teleport stopped: {message}\nStop Play to return safely.` else `Teleport cancelled: {message}`
		detail.Size = UDim2.new(1, -48, 0, 110)
		if not stopRequired then
			local failedGeneration = generation
			task.delay(6, function()
				if generation == failedGeneration then
					dismiss()
				end
			end)
		end
	end
	local function arrive()
		if animation then
			animation:Disconnect()
			animation = nil
		end
		arriving, screen = screen, nil
		if arriving then
			arriving.Parent = nil
		end
	end
	return { show = show, fail = fail, dismiss = dismiss, arrive = arrive, get = function()
		return screen
	end }
end

return Gui
