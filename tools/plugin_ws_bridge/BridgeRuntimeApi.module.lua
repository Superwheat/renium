--!nocheck

local BridgeRuntimeApi = {}

local CONSOLE_BUFFER_LIMIT = 1000
local CLIENT_RUNNER_NAME = "__ReniumClientRunner"
local SERVER_RUNNER_NAME = "__ReniumServerRunner"
local MOUSE_PROBE_NAME = "__ReniumMouseProbe"
local runnerSequence = 0

local function consoleTypeName(messageType)
	local text = tostring(messageType or "")
	local dot = string.find(text, "%.[^%.]*$")
	if dot ~= nil then
		return string.sub(text, dot + 1)
	end
	return text
end

local function compactInstancePath(instance)
	local parts = {}
	local current = instance
	while current ~= nil and current ~= game do
		table.insert(parts, 1, current.Name)
		current = current.Parent
	end
	return "game." .. table.concat(parts, ".")
end

local function setRunnerSource(runner, code)
	runnerSequence += 1
	pcall(function()
		runner.Enabled = false
	end)
	runner.Source = ("-- Renium run %d %.6f\n%s"):format(runnerSequence, os.clock(), code)
	pcall(function()
		runner.Enabled = true
	end)
end

local function serializeApiValue(value, depth)
	depth = depth or 0
	if depth > 6 then
		return tostring(value)
	end
	local valueType = typeof(value)
	if value == nil or valueType == "boolean" or valueType == "number" or valueType == "string" then
		return value
	end
	if valueType == "Instance" then
		return {
			_type = "Instance",
			name = value.Name,
			className = value.ClassName,
			path = compactInstancePath(value),
		}
	end
	if valueType == "Vector2" then
		return { _type = "Vector2", x = value.X, y = value.Y }
	end
	if valueType == "Vector3" then
		return { _type = "Vector3", x = value.X, y = value.Y, z = value.Z }
	end
	if valueType == "Color3" then
		return { _type = "Color3", r = value.R, g = value.G, b = value.B }
	end
	if valueType == "CFrame" then
		return { _type = "CFrame", value = { value:GetComponents() } }
	end
	if valueType == "EnumItem" then
		return { _type = "EnumItem", value = tostring(value) }
	end
	if valueType == "table" then
		local out = {}
		local count = 0
		for key, nested in pairs(value) do
			count += 1
			if count > 128 then
				out._truncated = true
				break
			end
			out[tostring(key)] = serializeApiValue(nested, depth + 1)
		end
		return out
	end
	return { _type = valueType, value = tostring(value) }
end

function BridgeRuntimeApi.create(plugin)
	local LogService = game:GetService("LogService")
	local RunService = game:GetService("RunService")
	local StudioTestService = game:GetService("StudioTestService")
	local consoleBuffer = {}
	local consoleSeq = 21335
	local playSession = {
		token = 0,
		active = false,
		starting = false,
		mode = nil,
		lastError = nil,
		lastResult = nil,
		lastStartedAt = 0,
		lastStoppedAt = 0,
	}

	local function appendConsoleEntry(message, messageType)
		consoleSeq += 1
		consoleBuffer[#consoleBuffer + 1] = {
			seq = consoleSeq,
			time = os.clock(),
			unix = os.time(),
			message = tostring(message),
			type = consoleTypeName(messageType),
		}
		while #consoleBuffer > CONSOLE_BUFFER_LIMIT do
			table.remove(consoleBuffer, 1)
		end
	end

	local okHistory, history = pcall(function()
		return LogService:GetLogHistory()
	end)
	if okHistory and type(history) == "table" then
		for _, entry in ipairs(history) do
			if type(entry) == "table" then
				appendConsoleEntry(entry.message or entry.Message or "", entry.messageType or entry.MessageType)
			end
		end
	end
	LogService.MessageOut:Connect(function(message, messageType)
		appendConsoleEntry(message, messageType)
	end)

	local api = {}

	local function currentRunStateText()
		local okState, state = pcall(function()
			return RunService.RunState
		end)
		if okState then
			return tostring(state)
		end
		return nil
	end

	local function currentStudioTestState()
		local editModeActive = nil
		local canLeaveTest = nil
		local okEditMode, editModeValue = pcall(function()
			return (StudioTestService :: any).EditModeActive
		end)
		if okEditMode and type(editModeValue) == "boolean" then
			editModeActive = editModeValue
		end
		local okCanLeave, canLeaveValue = pcall(function()
			return (StudioTestService :: any):CanLeaveTest()
		end)
		if okCanLeave and type(canLeaveValue) == "boolean" then
			canLeaveTest = canLeaveValue
		end
		return {
			editModeActive = editModeActive,
			canLeaveTest = canLeaveTest,
		}
	end

	local function getInstanceDebugId(instance)
		local ok, debugId = pcall(function()
			return instance:GetDebugId(32)
		end)
		if ok and type(debugId) == "string" then
			return debugId
		end
		return nil
	end

	local function parseGuiSegment(text)
		local name, ordinalText = string.match(text, "^(.-)%[(%d+)%]$")
		if name ~= nil and name ~= "" then
			return name, tonumber(ordinalText)
		end
		return text, nil
	end

	local function sameNameChildren(parent, name)
		local matches = {}
		for _, child in ipairs(parent:GetChildren()) do
			if child.Name == name then
				matches[#matches + 1] = child
			end
		end
		return matches
	end

	local function isEffectivelyVisible(instance)
		local current = instance
		while current ~= nil do
			if current:IsA("GuiObject") then
				if current.Visible ~= true then
					return false
				end
			elseif current:IsA("ScreenGui") then
				return current.Enabled == true
			end
			current = current.Parent
		end
		return false
	end

	local function guiCenterOnScreen(instance, insetX, insetY)
		local absPos = instance.AbsolutePosition
		local absSize = instance.AbsoluteSize
		local centerX = absPos.X + absSize.X / 2
		local centerY = absPos.Y + absSize.Y / 2
		local camera = game:GetService("Workspace").CurrentCamera
		if camera ~= nil then
			local viewport = camera.ViewportSize
			local viewportX = centerX + insetX
			local viewportY = centerY + insetY
			if viewportX < 0 or viewportY < 0 or viewportX > viewport.X or viewportY > viewport.Y then
				return false
			end
		end
		local current = instance.Parent
		while current ~= nil and not current:IsA("ScreenGui") do
			if current:IsA("GuiObject") and (current.ClipsDescendants == true or current:IsA("ScrollingFrame")) then
				local clipPos = current.AbsolutePosition
				local clipSize = current.AbsoluteSize
				if
					centerX < clipPos.X
					or centerX > clipPos.X + clipSize.X
					or centerY < clipPos.Y
					or centerY > clipPos.Y + clipSize.Y
				then
					return false
				end
			end
			current = current.Parent
		end
		return true
	end

	local function scrollGuiIntoView(instance)
		for _ = 1, 3 do
			local changed = false
			local centerX = instance.AbsolutePosition.X + instance.AbsoluteSize.X / 2
			local centerY = instance.AbsolutePosition.Y + instance.AbsoluteSize.Y / 2
			local current = instance.Parent
			while current ~= nil and not current:IsA("ScreenGui") do
				if current:IsA("ScrollingFrame") then
					local framePos = current.AbsolutePosition
					local frameSize = current.AbsoluteSize
					if
						centerX < framePos.X
						or centerX > framePos.X + frameSize.X
						or centerY < framePos.Y
						or centerY > framePos.Y + frameSize.Y
					then
						local canvas = current.CanvasPosition
						local canvasSize = current.AbsoluteCanvasSize
						local targetX = canvas.X + (centerX - framePos.X) - frameSize.X / 2
						local targetY = canvas.Y + (centerY - framePos.Y) - frameSize.Y / 2
						local maxX = math.max(canvasSize.X - frameSize.X, 0)
						local maxY = math.max(canvasSize.Y - frameSize.Y, 0)
						current.CanvasPosition =
							Vector2.new(math.clamp(targetX, 0, maxX), math.clamp(targetY, 0, maxY))
						changed = true
					end
				end
				current = current.Parent
			end
			if not changed then
				break
			end
			task.wait()
		end
	end

	local function guiOrdinalPath(root, instance)
		local parts = {}
		local current = instance
		while current ~= nil and current ~= root do
			local parent = current.Parent
			if parent == nil then
				break
			end
			local siblings = sameNameChildren(parent, current.Name)
			local segment = current.Name
			if #siblings > 1 then
				for position, sibling in ipairs(siblings) do
					if sibling == current then
						segment = ("%s[%d]"):format(current.Name, position)
						break
					end
				end
			end
			table.insert(parts, 1, segment)
			current = parent
		end
		return table.concat(parts, ".")
	end

	local function guiBoundsResult(current, root, matchedCount)
		local absPos = current.AbsolutePosition
		local absSize = current.AbsoluteSize
		local insetX, insetY = 0, 0
		local screenGui = current:FindFirstAncestorWhichIsA("ScreenGui")
		if screenGui == nil or screenGui.IgnoreGuiInset ~= true then
			local okInset, topLeft = pcall(function()
				return (game:GetService("GuiService") :: any):GetGuiInset()
			end)
			if okInset and typeof(topLeft) == "Vector2" then
				insetX = topLeft.X
				insetY = topLeft.Y
			end
		end
		local camera = game:GetService("Workspace").CurrentCamera
		local viewport = if camera ~= nil then camera.ViewportSize else Vector2.new(0, 0)
		return {
			ok = true,
			x = absPos.X + insetX + absSize.X / 2,
			y = absPos.Y + insetY + absSize.Y / 2,
			left = absPos.X + insetX,
			top = absPos.Y + insetY,
			width = absSize.X,
			height = absSize.Y,
			visible = isEffectivelyVisible(current),
			onScreen = guiCenterOnScreen(current, insetX, insetY),
			viewportWidth = viewport.X,
			viewportHeight = viewport.Y,
			fullName = current:GetFullName(),
			ordinalPath = guiOrdinalPath(root, current),
			id = getInstanceDebugId(current),
			matchedCount = matchedCount,
		}
	end

	function api.getGuiBounds(params)
		params = params or {}
		local idText = tostring(params.id or "")
		if idText ~= "" then
			local localPlayerForId = game:GetService("Players").LocalPlayer
			if localPlayerForId == nil then
				return { ok = false, error = "LocalPlayer is not available" }
			end
			local playerGui = localPlayerForId:FindFirstChildOfClass("PlayerGui")
			if playerGui == nil then
				return { ok = false, error = "PlayerGui is not available" }
			end
			for _, descendant in ipairs(playerGui:GetDescendants()) do
				if descendant:IsA("GuiObject") and getInstanceDebugId(descendant) == idText then
					if params.scroll == true then
						scrollGuiIntoView(descendant)
					end
					return guiBoundsResult(descendant, playerGui, 1)
				end
			end
			return { ok = false, error = ("No PlayerGui descendant has id %s"):format(idText) }
		end
		local pathText = tostring(params.path or "")
		if pathText == "" then
			return { ok = false, error = "Missing gui path or id" }
		end
		local segments = {}
		for segment in string.gmatch(pathText, "[^%.]+") do
			segments[#segments + 1] = segment
		end
		local localPlayer = game:GetService("Players").LocalPlayer
		local root
		local index = 1
		local firstName = parseGuiSegment(segments[1] or "")
		if firstName == "game" then
			root = game
			index = 2
		elseif firstName == "Workspace" then
			root = game:GetService("Workspace")
			index = 2
		elseif firstName == "LocalPlayer" or firstName == "PlayerGui" then
			if localPlayer == nil then
				return { ok = false, error = "LocalPlayer is not available" }
			end
			if firstName == "LocalPlayer" then
				root = localPlayer
			else
				root = localPlayer:FindFirstChildOfClass("PlayerGui")
				if root == nil then
					return { ok = false, error = "PlayerGui is not available" }
				end
			end
			index = 2
		else
			if localPlayer == nil then
				return { ok = false, error = "LocalPlayer is not available" }
			end
			root = localPlayer:FindFirstChildOfClass("PlayerGui")
			if root == nil then
				return { ok = false, error = "PlayerGui is not available" }
			end
		end

		local frontier = { root }
		while index <= #segments do
			local name, ordinal = parseGuiSegment(segments[index])
			local nextFrontier = {}
			for _, node in ipairs(frontier) do
				local matches = sameNameChildren(node, name)
				if ordinal ~= nil then
					if matches[ordinal] ~= nil then
						nextFrontier[#nextFrontier + 1] = matches[ordinal]
					end
				else
					for _, match in ipairs(matches) do
						nextFrontier[#nextFrontier + 1] = match
					end
				end
			end
			if #nextFrontier == 0 then
				return {
					ok = false,
					error = ("Path segment '%s' matched nothing under %s"):format(
						segments[index],
						pathText
					),
				}
			end
			if #nextFrontier > 64 then
				return {
					ok = false,
					error = ("Path '%s' is too ambiguous (over 64 matches at segment '%s'); add [n] ordinals"):format(
						pathText,
						segments[index]
					),
				}
			end
			frontier = nextFrontier
			index += 1
		end

		local candidates = {}
		for _, node in ipairs(frontier) do
			if node:IsA("GuiObject") then
				candidates[#candidates + 1] = node
			end
		end
		if #candidates == 0 then
			return { ok = false, error = ("Path '%s' matched no GuiObject"):format(pathText) }
		end

		local matchedCount = #candidates
		local current
		if matchedCount == 1 then
			current = candidates[1]
		else
			local visibleCandidates = {}
			for _, candidate in ipairs(candidates) do
				if isEffectivelyVisible(candidate) then
					visibleCandidates[#visibleCandidates + 1] = candidate
				end
			end
			if #visibleCandidates == 1 then
				current = visibleCandidates[1]
			else
				local pool = if #visibleCandidates > 0 then visibleCandidates else candidates
				local listing = {}
				local candidateInfos = {}
				for _, candidate in ipairs(pool) do
					local candidateId = getInstanceDebugId(candidate)
					local candidateVisible = isEffectivelyVisible(candidate)
					local candidatePath = guiOrdinalPath(root, candidate)
					listing[#listing + 1] = ("%s (visible=%s, id=%s)"):format(
						candidatePath,
						tostring(candidateVisible),
						tostring(candidateId)
					)
					candidateInfos[#candidateInfos + 1] = {
						ordinalPath = candidatePath,
						id = candidateId,
						visible = candidateVisible,
						className = candidate.ClassName,
					}
				end
				local reason = if #visibleCandidates == 0
					then "none of them is visible"
					else ("%d of them are visible"):format(#visibleCandidates)
				return {
					ok = false,
					error = ("Path '%s' matched %d elements and %s. Disambiguate with [n] ordinals or press by id: %s"):format(
						pathText,
						matchedCount,
						reason,
						table.concat(listing, "; ")
					),
					candidates = candidateInfos,
				}
			end
		end
		if params.scroll == true then
			scrollGuiIntoView(current)
		end
		return guiBoundsResult(current, root, matchedCount)
	end

	function api.getGuiInventory(params)
		params = params or {}
		local limit = math.clamp(tonumber(params.limit) or 200, 1, 500)
		local localPlayer = game:GetService("Players").LocalPlayer
		if localPlayer == nil then
			return { ok = false, error = "LocalPlayer is not available" }
		end
		local playerGui = localPlayer:FindFirstChildOfClass("PlayerGui")
		if playerGui == nil then
			return { ok = false, error = "PlayerGui is not available" }
		end
		local insetX, insetY = 0, 0
		local okInset, topLeft = pcall(function()
			return (game:GetService("GuiService") :: any):GetGuiInset()
		end)
		if okInset and typeof(topLeft) == "Vector2" then
			insetX = topLeft.X
			insetY = topLeft.Y
		end
		local items = {}
		local truncated = false
		local includeOffscreen = params.includeOffscreen == true
		for _, descendant in ipairs(playerGui:GetDescendants()) do
			if (descendant:IsA("GuiButton") or descendant:IsA("TextBox")) and isEffectivelyVisible(descendant) then
				local screenGui = descendant:FindFirstAncestorWhichIsA("ScreenGui")
				local offX, offY = insetX, insetY
				if screenGui ~= nil and screenGui.IgnoreGuiInset == true then
					offX, offY = 0, 0
				end
				if not includeOffscreen and not guiCenterOnScreen(descendant, offX, offY) then
					continue
				end
				if #items >= limit then
					truncated = true
					break
				end
				local absPos = descendant.AbsolutePosition
				local absSize = descendant.AbsoluteSize
				local text = nil
				if descendant:IsA("TextButton") or descendant:IsA("TextBox") then
					text = string.sub(descendant.Text, 1, 60)
				end
				items[#items + 1] = {
					p = guiOrdinalPath(playerGui, descendant),
					c = descendant.ClassName,
					t = text,
					id = getInstanceDebugId(descendant),
					x = math.floor(absPos.X + offX + absSize.X / 2 + 0.5),
					y = math.floor(absPos.Y + offY + absSize.Y / 2 + 0.5),
					w = math.floor(absSize.X + 0.5),
					h = math.floor(absSize.Y + 0.5),
				}
			end
		end
		return { ok = true, items = items, count = #items, truncated = truncated }
	end

	function api.getWorldPoint(params)
		params = params or {}
		local pathText = tostring(params.path or "")
		if pathText == "" then
			return { ok = false, error = "Missing world instance path" }
		end
		local segments = {}
		for segment in string.gmatch(pathText, "[^%.]+") do
			segments[#segments + 1] = segment
		end
		local current = game
		local index = 1
		local firstName = parseGuiSegment(segments[1] or "")
		if firstName == "game" then
			index = 2
		elseif firstName == "Workspace" or firstName == "workspace" then
			current = game:GetService("Workspace")
			index = 2
		else
			current = game:GetService("Workspace")
		end
		while index <= #segments do
			local name, ordinal = parseGuiSegment(segments[index])
			local matches = sameNameChildren(current, name)
			local nextInstance = if ordinal ~= nil then matches[ordinal] else matches[1]
			if nextInstance == nil then
				return {
					ok = false,
					error = ("Path segment '%s' not found under %s"):format(segments[index], current:GetFullName()),
				}
			end
			if ordinal == nil and #matches > 1 then
				return {
					ok = false,
					error = ("Segment '%s' matched %d instances under %s; add [n] ordinals"):format(
						name,
						#matches,
						current:GetFullName()
					),
				}
			end
			current = nextInstance
			index += 1
		end
		local position
		if current:IsA("BasePart") then
			position = current.Position
		elseif current:IsA("Model") then
			position = current:GetPivot().Position
		else
			return { ok = false, error = current:GetFullName() .. " is not a BasePart or Model" }
		end
		local camera = game:GetService("Workspace").CurrentCamera
		if camera == nil then
			return { ok = false, error = "No CurrentCamera" }
		end
		local point, inFront = camera:WorldToViewportPoint(position)
		local viewport = camera.ViewportSize
		local onScreen = inFront
			and point.X >= 0
			and point.Y >= 0
			and point.X <= viewport.X
			and point.Y <= viewport.Y
		return {
			ok = true,
			x = point.X,
			y = point.Y,
			depth = point.Z,
			inFront = inFront,
			onScreen = onScreen,
			viewportWidth = viewport.X,
			viewportHeight = viewport.Y,
			fullName = current:GetFullName(),
			worldPosition = { position.X, position.Y, position.Z },
		}
	end

	function api.getMouseLocation(_params)
		local localPlayer = game:GetService("Players").LocalPlayer
		if localPlayer == nil then
			return { ok = false, error = "LocalPlayer is not available" }
		end
		local playerScripts = localPlayer:FindFirstChildOfClass("PlayerScripts")
			or localPlayer:WaitForChild("PlayerScripts", 2)
		if playerScripts == nil then
			return { ok = false, error = "PlayerScripts is not available" }
		end
		local probe = playerScripts:FindFirstChild(MOUSE_PROBE_NAME)
		if probe == nil then
			probe = Instance.new("LocalScript")
			probe.Name = MOUSE_PROBE_NAME
			setRunnerSource(
				probe,
				[==[
local probe = script
local UserInputService = game:GetService("UserInputService")
local GuiService = game:GetService("GuiService")
UserInputService.InputBegan:Connect(function(input)
	if input.UserInputType == Enum.UserInputType.MouseButton3 then
		local inset = GuiService:GetGuiInset()
		probe:SetAttribute("ProbeX", input.Position.X + inset.X)
		probe:SetAttribute("ProbeY", input.Position.Y + inset.Y)
		probe:SetAttribute("ProbeSeq", (probe:GetAttribute("ProbeSeq") or 0) + 1)
	end
end)
game:GetService("RunService").RenderStepped:Connect(function()
	local location = UserInputService:GetMouseLocation()
	probe:SetAttribute("MouseX", location.X)
	probe:SetAttribute("MouseY", location.Y)
end)
]==]
			)
			probe.Parent = playerScripts
		end
		local x = probe:GetAttribute("MouseX")
		local y = probe:GetAttribute("MouseY")
		local camera = game:GetService("Workspace").CurrentCamera
		local viewport = if camera ~= nil then camera.ViewportSize else Vector2.new(0, 0)
		if type(x) ~= "number" or type(y) ~= "number" then
			return {
				ok = false,
				error = "Mouse probe is not ready yet",
				viewportWidth = viewport.X,
				viewportHeight = viewport.Y,
			}
		end
		return {
			ok = true,
			x = x,
			y = y,
			probeSeq = probe:GetAttribute("ProbeSeq") or 0,
			probeX = probe:GetAttribute("ProbeX"),
			probeY = probe:GetAttribute("ProbeY"),
			viewportWidth = viewport.X,
			viewportHeight = viewport.Y,
		}
	end

	function api.getConsoleOutput(params)
		params = params or {}
		local limit = math.clamp(tonumber(params.limit) or 200, 1, CONSOLE_BUFFER_LIMIT)
		local sinceSeq = tonumber(params.sinceSeq) or tonumber(params.since) or tonumber(params.cursorSeq) or 0
		local entries = {}
		local startIndex = math.max(1, #consoleBuffer - limit + 1)
		for i = startIndex, #consoleBuffer do
			local entry = consoleBuffer[i]
			if entry ~= nil and (entry.seq or 0) > sinceSeq then
				entries[#entries + 1] = entry
			end
		end
		local truncated = false
		if startIndex > 1 then
			local previous = consoleBuffer[startIndex - 1]
			truncated = previous ~= nil and (previous.seq or 0) > sinceSeq
		end
		if params.clear == true then
			table.clear(consoleBuffer)
		end
		return {
			ok = true,
			entries = entries,
			count = #entries,
			nextSeq = consoleSeq,
			truncated = truncated,
		}
	end

	function api.executeLuau(params)
		params = params or {}
		local code = tostring(params.code or "")
		if code == "" then
			return { ok = false, error = "Missing Luau code" }
		end

		local context = string.lower(tostring(params.context or params.target or "plugin"))
		if context == "client" or context == "local" then
			local okClient, isClient = pcall(function()
				return RunService:IsClient()
			end)
			local okRunning, running = pcall(function()
				return RunService:IsRunning()
			end)
			if okClient and isClient == true and okRunning and running == true then
				local directParams = {}
				for key, value in pairs(params) do
					directParams[key] = value
				end
				directParams.context = "plugin"
				local directResult = api.executeLuau(directParams)
				if type(directResult) == "table" then
					if directResult.ok ~= false then
						directResult.context = "client"
						directResult.direct = true
						return directResult
					end
					if not string.find(tostring(directResult.error or ""), "loadstring", 1, true) then
						directResult.context = "client"
						directResult.direct = true
						return directResult
					end
				end
			end

			local players = game:GetService("Players")
			local localPlayer = players.LocalPlayer
			if localPlayer == nil then
				return { ok = false, error = "Play client LocalPlayer is not available" }
			end
			local playerScripts = localPlayer:FindFirstChildOfClass("PlayerScripts") or localPlayer:WaitForChild("PlayerScripts", 2)
			if playerScripts == nil then
				return { ok = false, error = "PlayerScripts is not available on the play client" }
			end

			local scriptInstance = playerScripts:FindFirstChild(CLIENT_RUNNER_NAME)
			if scriptInstance ~= nil and not scriptInstance:IsA("LocalScript") then
				scriptInstance:Destroy()
				scriptInstance = nil
			end
			if scriptInstance == nil then
				scriptInstance = Instance.new("LocalScript")
				scriptInstance.Name = CLIENT_RUNNER_NAME
				scriptInstance.Parent = playerScripts
			end
			setRunnerSource(scriptInstance, code)

			return {
				ok = true,
				context = "client",
				runner = true,
				path = compactInstancePath(scriptInstance),
			}
		end

		if type(loadstring) ~= "function" then
			return { ok = false, error = "loadstring is not available in this Studio session" }
		end

		local chunkName = tostring(params.chunkName or params.name or "Renium")
		if chunkName == "" then
			chunkName = "Renium"
		end

		local loadOk, loadedChunk, loadedError = pcall(loadstring, code, chunkName)
		local chunk = loadedChunk
		local compileError = loadedError
		if not loadOk then
			local retryOk, retryChunk, retryError = pcall(loadstring, code)
			if retryOk then
				chunk = retryChunk
				compileError = retryError
			else
				chunk = nil
				compileError = retryChunk
			end
		end
		if chunk == nil then
			local errorText = tostring(compileError)
			local okRunning, running = pcall(function()
				return RunService:IsRunning()
			end)
			local okServer, isServer = pcall(function()
				return RunService:IsServer()
			end)
			if string.find(errorText, "loadstring", 1, true) and okRunning and running == true and okServer and isServer == true then
				local serverScriptService = game:GetService("ServerScriptService")
				local scriptInstance = serverScriptService:FindFirstChild(SERVER_RUNNER_NAME)
				if scriptInstance ~= nil and not scriptInstance:IsA("Script") then
					scriptInstance:Destroy()
					scriptInstance = nil
				end
				if scriptInstance == nil then
					scriptInstance = Instance.new("Script")
					scriptInstance.Name = SERVER_RUNNER_NAME
					scriptInstance.RunContext = Enum.RunContext.Server
					scriptInstance.Parent = serverScriptService
				end
				setRunnerSource(scriptInstance, code)

				return {
					ok = true,
					context = "server",
					runner = true,
					path = compactInstancePath(scriptInstance),
				}
			end
			return { ok = false, error = tostring(compileError) }
		end

		local output = {}
		local function capture(kind, ...)
			local values = table.pack(...)
			local parts = table.create(values.n)
			for i = 1, values.n do
				parts[i] = tostring(values[i])
			end
			output[#output + 1] = {
				type = kind,
				message = table.concat(parts, "\t"),
			}
		end

		local baseEnv = getfenv(0)
		local env = setmetatable({
			plugin = plugin,
			print = function(...)
				capture("print", ...)
				print(...)
			end,
			warn = function(...)
				capture("warn", ...)
				warn(...)
			end,
		}, { __index = baseEnv })
		pcall(setfenv, chunk, env)

		local packed = table.pack(pcall(chunk))
		if packed[1] ~= true then
			return {
				ok = false,
				error = tostring(packed[2]),
				output = output,
			}
		end

		local results = {}
		for i = 2, packed.n do
			results[#results + 1] = serializeApiValue(packed[i])
		end
		return {
			ok = true,
			results = results,
			output = output,
		}
	end

	function api.isPlayModeRunning()
		local studioTestState = currentStudioTestState()
		local okEdit, editMode = pcall(function()
			return RunService:IsEdit()
		end)
		local okRunning, running = pcall(function()
			return RunService:IsRunning()
		end)
		local okState, state = pcall(function()
			return RunService.RunState
		end)
		if okState and state == Enum.RunState.Stopped and studioTestState.editModeActive ~= false then
			return false
		end
		if studioTestState.editModeActive == true then
			return false
		end
		if okRunning and running == true then
			return true
		end
		if studioTestState.editModeActive == false then
			return true
		end
		if okEdit and editMode == true then
			return false
		end
		if okEdit and editMode == false then
			return true
		end
		return okState and state ~= nil and state ~= Enum.RunState.Stopped and (not okEdit or editMode == false)
	end

	local function waitForRunning(timeoutSeconds)
		local deadline = os.clock() + timeoutSeconds
		while os.clock() < deadline do
			if api.isPlayModeRunning() then
				playSession.starting = false
				return true
			end
			if playSession.starting ~= true and playSession.lastError ~= nil then
				return false
			end
			task.wait(0.05)
		end
		local running = api.isPlayModeRunning()
		if running then
			playSession.starting = false
		end
		return running
	end

	local function waitForStopped(timeoutSeconds)
		local deadline = os.clock() + timeoutSeconds
		while os.clock() < deadline do
			if not api.isPlayModeRunning() then
				return true
			end
			task.wait(0.05)
		end
		return not api.isPlayModeRunning()
	end

	local function executeStudioTest(token, mode, testArgs, numPlayers)
		workspace:SetAttribute("__ReniumPlace", game.Name)
		local ok, result = pcall(function()
			if mode == "run" then
				return StudioTestService:ExecuteRunModeAsync(testArgs)
			end
			if mode == "multi" then
				return (StudioTestService :: any):ExecuteMultiplayerTestAsync(numPlayers, testArgs)
			end
			return StudioTestService:ExecutePlayModeAsync(testArgs)
		end)
		if playSession.token ~= token then
			return
		end
		playSession.active = false
		playSession.starting = false
		playSession.lastStoppedAt = os.clock()
		if ok then
			playSession.lastResult = serializeApiValue(result)
			playSession.lastError = nil
		else
			playSession.lastResult = nil
			playSession.lastError = tostring(result)
		end
	end

	function api.startStopPlay(params)
		params = params or {}
		local shouldStart = params.start == true or params.isStart == true or params.playing == true
		local shouldStop = params.stop == true or params.isStart == false or params.playing == false
		if shouldStart and shouldStop then
			return { ok = false, error = "Conflicting play state request" }
		end

		local action = "status"
		if shouldStart then
			action = "start"
			if playSession.starting == true
				and not api.isPlayModeRunning()
				and os.clock() - (playSession.lastStartedAt or 0) > 60
			then
				playSession.token += 1
				playSession.starting = false
				playSession.active = false
			end
			if not api.isPlayModeRunning() and playSession.starting ~= true then
				local numPlayers = tonumber(params.players)
				if numPlayers ~= nil then
					numPlayers = math.clamp(math.floor(numPlayers), 1, 8)
				end
				playSession.token += 1
				playSession.active = true
				playSession.starting = true
				if params.mode == "run" then
					playSession.mode = "run"
				elseif numPlayers ~= nil and numPlayers >= 1 then
					playSession.mode = "multi"
				else
					playSession.mode = "play"
				end
				playSession.lastError = nil
				playSession.lastResult = nil
				playSession.lastStartedAt = os.clock()
				local token = playSession.token
				local mode = playSession.mode
				local testArgs = params.args or ""
				task.spawn(function()
					executeStudioTest(token, mode, testArgs, numPlayers)
				end)
			end
			local timeoutSeconds = math.clamp(tonumber(params.timeoutSeconds) or 2, 0.1, 10)
			local running = waitForRunning(timeoutSeconds)
			if not running and playSession.lastError ~= nil then
				return {
					ok = false,
					action = action,
					error = playSession.lastError,
					running = false,
					starting = playSession.starting,
					mode = playSession.mode,
					runState = currentRunStateText(),
				}
			end
			if not running then
				return {
					ok = false,
					action = action,
					error = "Timed out waiting for Studio test session to start",
					running = false,
					starting = playSession.starting,
					mode = playSession.mode,
					runState = currentRunStateText(),
				}
			end
		elseif shouldStop then
			action = "stop"
			local attempts = {}
			local function attempt(label, callback)
				local attemptOk, attemptErr = pcall(callback)
				local errorMessage = nil
				if not attemptOk then
					errorMessage = tostring(attemptErr)
				end
				attempts[#attempts + 1] = {
					method = label,
					ok = attemptOk,
					error = errorMessage,
				}
				return attemptOk
			end

			if api.isPlayModeRunning() or playSession.active == true or playSession.starting == true then
				attempt("EndTest", function()
					(StudioTestService :: any):EndTest(true)
				end)
			end
			if api.isPlayModeRunning() then
				attempt("EditModeActive", function()
					(StudioTestService :: any).EditModeActive = true
				end)
			end
			if api.isPlayModeRunning() then
				attempt("RunService.Stop", function()
					(RunService :: any):Stop()
				end)
			end
			local stopped = waitForStopped(math.clamp(tonumber(params.timeoutSeconds) or 2, 0.1, 10))
			if stopped then
				playSession.active = false
				playSession.starting = false
				playSession.lastStoppedAt = os.clock()
			else
				return {
					ok = false,
					action = action,
					error = "Timed out waiting for Studio test session to stop",
					attempts = attempts,
					running = api.isPlayModeRunning(),
					starting = playSession.starting,
					mode = playSession.mode,
					runState = currentRunStateText(),
				}
			end
		end

		return {
			ok = true,
			action = action,
			running = api.isPlayModeRunning(),
			starting = playSession.starting,
			mode = playSession.mode,
			lastError = playSession.lastError,
			lastResult = playSession.lastResult,
			runState = currentRunStateText(),
			studioTest = currentStudioTestState(),
		}
	end

	function api.bindRunStateHidden(ui)
		local function update()
			ui.setPlayModeHidden(api.isPlayModeRunning())
		end
		pcall(function()
			(StudioTestService :: any):GetPropertyChangedSignal("EditModeActive"):Connect(update)
		end)
		pcall(function()
			RunService:GetPropertyChangedSignal("RunState"):Connect(update)
		end)
		update()
	end

	return api
end

return BridgeRuntimeApi
