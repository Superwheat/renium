

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
	local deviceSimulatorReadyAt = os.clock() + 4
	local captureProbeGui = nil
	local captureProbeFrame = nil

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

	local function executeWithRunner(parent, className, baseName, runContext, code, timeoutSeconds, context)
		local scriptInstance = Instance.new(className)
		scriptInstance.Name = baseName .. "_" .. tostring(runnerSequence + 1)
		pcall(function()
			scriptInstance.Enabled = false
		end)
		if runContext ~= nil then
			scriptInstance.RunContext = runContext
		end
		local resultEvent = Instance.new("BindableEvent")
		resultEvent.Name = "__ReniumResult"
		resultEvent.Parent = scriptInstance
		local status = nil
		local values = nil
		local resultConnection = resultEvent.Event:Connect(function(nextStatus, ...)
			if status == nil then
				status = tostring(nextStatus)
				values = table.pack(...)
			end
		end)
		local logError = nil
		local logConnection = LogService.MessageOut:Connect(function(message, messageType)
			if logError == nil
				and string.find(tostring(message), scriptInstance.Name, 1, true)
				and string.find(string.lower(consoleTypeName(messageType)), "error", 1, true)
			then
				logError = tostring(message)
			end
		end)
		scriptInstance.Parent = parent
		local path = compactInstancePath(scriptInstance)
		local wrapped = ([==[
local resultEvent = script:FindFirstChild("__ReniumResult")
if resultEvent == nil then return end
local output = {}
local basePrint = print
local baseWarn = warn
local function capture(kind, ...)
	local values = table.pack(...)
	local parts = table.create(values.n)
	for index = 1, values.n do parts[index] = tostring(values[index]) end
	output[#output + 1] = { type = kind, message = table.concat(parts, "\t") }
	if kind == "print" then basePrint(...) else baseWarn(...) end
end
local print = function(...) capture("print", ...) end
local warn = function(...) capture("warn", ...) end
local finished = false
local worker
worker = task.spawn(function()
	local packed = table.pack(xpcall(function()
%s
	end, function(message)
		local ok, trace = pcall(function() return debug.traceback(tostring(message), 2) end)
		return if ok then trace else tostring(message)
	end))
	if finished then return end
	finished = true
	if packed[1] then
		resultEvent:Fire("ok", output, table.unpack(packed, 2, packed.n))
	else
		resultEvent:Fire("error", tostring(packed[2]), output)
	end
end)
task.delay(%.6f, function()
	if finished then return end
	finished = true
	pcall(task.cancel, worker)
	resultEvent:Fire("timeout", output)
end)
]==]):format(code, timeoutSeconds)
		setRunnerSource(scriptInstance, wrapped)
		local deadline = os.clock() + timeoutSeconds + 1
		while status == nil and logError == nil and scriptInstance.Parent ~= nil and os.clock() < deadline do
			task.wait()
		end
		resultConnection:Disconnect()
		logConnection:Disconnect()
		pcall(function()
			scriptInstance.Enabled = false
		end)
		scriptInstance:Destroy()
		if logError ~= nil then
			return { ok = false, error = logError, output = {}, runner = true, path = path, context = context }
		end
		if status == "error" then
			return {
				ok = false,
				error = tostring(values and values[1] or "Luau runner failed"),
				output = if values and type(values[2]) == "table" then values[2] else {},
				runner = true,
				path = path,
				context = context,
			}
		end
		if status ~= "ok" then
			return {
				ok = false,
				error = ("Luau runner timed out after %.1fs and was stopped"):format(timeoutSeconds),
				timedOut = true,
				stopped = true,
				output = if values and type(values[1]) == "table" then values[1] else {},
				runner = true,
				path = path,
				context = context,
			}
		end
		local results = {}
		if values ~= nil then
			for index = 2, values.n do
				results[#results + 1] = serializeApiValue(values[index])
			end
		end
		return {
			ok = true,
			results = results,
			output = if values and type(values[1]) == "table" then values[1] else {},
			runner = true,
			path = path,
			context = context,
		}
	end

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

	function api.captureViewportProbe(params)
		params = params or {}
		local action = string.lower(tostring(params.action or "start"))
		if action == "stop" or action == "clear" then
			if captureProbeGui ~= nil then
				captureProbeGui:Destroy()
			end
			captureProbeGui = nil
			captureProbeFrame = nil
			task.wait()
			return { ok = true, action = "stop" }
		end
		local colors = params.colors
		if type(colors) ~= "table" or #colors ~= 16 then
			return { ok = false, error = "Capture probe requires 16 colors" }
		end
		if action == "start" then
			if captureProbeGui ~= nil then
				captureProbeGui:Destroy()
			end
			local screen = Instance.new("ScreenGui")
			screen.Name = "__ReniumCaptureProbe"
			screen.Archivable = false
			screen.DisplayOrder = 2147483647
			screen.IgnoreGuiInset = true
			screen.ResetOnSpawn = false
			screen.ZIndexBehavior = Enum.ZIndexBehavior.Global
			pcall(function()
				screen.ScreenInsets = Enum.ScreenInsets.None
				screen.ClipToDeviceSafeArea = false
				screen.SafeAreaCompatibility = Enum.SafeAreaCompatibility.None
			end)
			local frame = Instance.new("Frame")
			frame.Name = "Probe"
			frame.Archivable = false
			frame.BackgroundTransparency = 1
			frame.BorderSizePixel = 0
			frame.ClipsDescendants = true
			frame.Size = UDim2.fromScale(1, 1)
			frame.ZIndex = 2147483647
			for index = 1, 16 do
				local packed = tonumber(colors[index]) or 0
				local tile = Instance.new("Frame")
				tile.Name = tostring(index)
				tile.Archivable = false
				tile.BackgroundColor3 = Color3.fromRGB(
					bit32.extract(packed, 16, 8),
					bit32.extract(packed, 8, 8),
					bit32.extract(packed, 0, 8)
				)
				tile.BackgroundTransparency = 0.98
				tile.BorderSizePixel = 0
				tile.Position = UDim2.fromScale(((index - 1) % 4) / 4, math.floor((index - 1) / 4) / 4)
				tile.Size = UDim2.fromScale(0.25, 0.25)
				tile.ZIndex = 2147483647
				tile.Parent = frame
			end
			frame.Parent = screen
			screen.Parent = game:GetService("CoreGui")
			captureProbeGui = screen
			captureProbeFrame = frame
		elseif action == "phase" then
			if captureProbeFrame == nil or captureProbeFrame.Parent == nil then
				return { ok = false, error = "Capture probe is not active" }
			end
			for index = 1, 16 do
				local packed = tonumber(colors[index]) or 0
				local tile = captureProbeFrame:FindFirstChild(tostring(index))
				if tile == nil then
					return { ok = false, error = "Capture probe tile is missing" }
				end
				tile.BackgroundColor3 = Color3.fromRGB(
					bit32.extract(packed, 16, 8),
					bit32.extract(packed, 8, 8),
					bit32.extract(packed, 0, 8)
				)
			end
		else
			return { ok = false, error = "Unknown capture probe action '" .. action .. "'" }
		end
		task.wait()
		return { ok = true, action = action }
	end

	function api.deviceSimulator(params)
		params = params or {}
		local action = string.lower(tostring(params.action or "status"))
		local service = game:GetService("StudioDeviceSimulatorService")

		local function enumName(value)
			local text = tostring(value)
			return string.match(text, "[^%.]+$") or text
		end

		local function deviceInfo(deviceId)
			local info = service:GetDeviceInfoAsync(deviceId)
			return {
				id = tostring(info.DeviceId or deviceId),
				name = tostring(info.Name or deviceId),
				form = enumName(info.DeviceForm),
				custom = info.IsCustom == true,
				width = tonumber(info.Width),
				height = tonumber(info.Height),
				pixelDensity = tonumber(info.PixelDensity),
				resolutionScale = tonumber(info.ResolutionScale),
				portraitKeyboardHeight = tonumber(info.PortraitKeyboardHeight),
				landscapeKeyboardHeight = tonumber(info.LandscapeKeyboardHeight),
			}
		end

		local function status()
			local camera = workspace.CurrentCamera
			local viewport = if camera ~= nil then camera.ViewportSize else Vector2.new(0, 0)
			local inactive = {
				ok = true,
				action = "status",
				simulating = false,
				viewport = { width = viewport.X, height = viewport.Y },
			}
			local okDevice, deviceId = pcall(function()
				return service:GetDeviceAsync()
			end)
			if not okDevice or type(deviceId) ~= "string" or deviceId == "" then
				return inactive
			end
			local okConfig, resolution, orientation, scalingMode, pixelDensity = pcall(function()
				return service:GetResolutionAsync(),
					service:GetOrientationAsync(),
					service:GetScalingModeAsync(),
					service:GetPixelDensityAsync()
			end)
			if not okConfig then
				return inactive
			end
			local result = {
				ok = true,
				action = "status",
				simulating = true,
				settleSeconds = math.max(0, deviceSimulatorReadyAt - os.clock()),
				viewport = { width = viewport.X, height = viewport.Y },
				deviceId = deviceId,
				orientation = enumName(orientation),
				scalingMode = enumName(scalingMode),
				resolution = {
					width = resolution.X,
					height = resolution.Y,
				},
				pixelDensity = pixelDensity,
			}
			if type(deviceId) == "string" and deviceId ~= "" then
				local okInfo, info = pcall(deviceInfo, deviceId)
				if okInfo then
					result.device = info
				end
			end
			return result
		end

		local function listDevices()
			local devices = {}
			for _, deviceId in ipairs(service:GetDeviceListAsync()) do
				local okInfo, info = pcall(deviceInfo, deviceId)
				if okInfo then
					devices[#devices + 1] = info
				else
					devices[#devices + 1] = { id = tostring(deviceId), name = tostring(deviceId) }
				end
			end
			return devices
		end

		local function normalized(value)
			return string.gsub(string.lower(tostring(value or "")), "[^%w]", "")
		end

		local function resolveDevice(requested, devices)
			local key = normalized(requested)
			if key == "" then
				return nil, "Missing device name or id"
			end
			for _, device in ipairs(devices) do
				if normalized(device.id) == key or normalized(device.name) == key then
					return device
				end
			end
			local matches = {}
			for _, device in ipairs(devices) do
				if string.find(normalized(device.id), key, 1, true)
					or string.find(normalized(device.name), key, 1, true)
				then
					matches[#matches + 1] = device
				end
			end
			if #matches == 1 then
				return matches[1]
			end
			if #matches == 0 then
				return nil, "Unknown device '" .. tostring(requested) .. "'"
			end
			local names = {}
			for _, device in ipairs(matches) do
				names[#names + 1] = device.name .. " (" .. device.id .. ")"
			end
			return nil, "Device '" .. tostring(requested) .. "' is ambiguous: " .. table.concat(names, ", ")
		end

		if action == "list" then
			local devices = listDevices()
			return { ok = true, action = "list", count = #devices, devices = devices }
		end

		if action == "status" or action == "get" then
			return status()
		end

		if action == "stop" or action == "reset" then
			local before = status()
			if before.simulating ~= true then
				return { ok = true, action = "stop", stopped = true, alreadyStopped = true }
			end
			local okStop, stopError = pcall(function()
				service:StopSimulationAsync()
			end)
			if not okStop and not string.find(string.lower(tostring(stopError)), "no device is active", 1, true) then
				return { ok = false, error = tostring(stopError) }
			end
			deviceSimulatorReadyAt = 0
			return { ok = true, action = "stop", stopped = true, alreadyStopped = false }
		end

		if action ~= "set" and action ~= "select" and action ~= "apply" then
			return { ok = false, error = "Unknown device action '" .. action .. "'" }
		end

		local selectedDevice = nil
		if params.device ~= nil and tostring(params.device) ~= "" then
			local device, resolveError = resolveDevice(params.device, listDevices())
			if device == nil then
				return { ok = false, error = resolveError }
			end
			selectedDevice = device
		end

		local orientation = nil
		if params.orientation ~= nil and tostring(params.orientation) ~= "" then
			local key = normalized(params.orientation)
			local orientations = {
				portrait = Enum.ScreenOrientation.Portrait,
				landscape = Enum.ScreenOrientation.LandscapeRight,
				landscaperight = Enum.ScreenOrientation.LandscapeRight,
				landscapeleft = Enum.ScreenOrientation.LandscapeLeft,
				landscapesensor = Enum.ScreenOrientation.LandscapeSensor,
				sensor = Enum.ScreenOrientation.Sensor,
			}
			orientation = orientations[key]
			if orientation == nil then
				return { ok = false, error = "Unknown orientation '" .. tostring(params.orientation) .. "'" }
			end
		end

		local scalingMode = nil
		if params.scalingMode ~= nil and tostring(params.scalingMode) ~= "" then
			local key = normalized(params.scalingMode)
			local scalingModes = {
				physical = Enum.DeviceSimulatorScalingMode.ScaleToPhysicalSize,
				scaletophysicalsize = Enum.DeviceSimulatorScalingMode.ScaleToPhysicalSize,
				actual = Enum.DeviceSimulatorScalingMode.ActualResolution,
				actualresolution = Enum.DeviceSimulatorScalingMode.ActualResolution,
				fit = Enum.DeviceSimulatorScalingMode.FitToWindow,
				fittowindow = Enum.DeviceSimulatorScalingMode.FitToWindow,
			}
			scalingMode = scalingModes[key]
			if scalingMode == nil then
				return { ok = false, error = "Unknown scaling mode '" .. tostring(params.scalingMode) .. "'" }
			end
		end

		local width = tonumber(params.width)
		local height = tonumber(params.height)
		if width ~= nil or height ~= nil then
			if width == nil or height == nil or width < 1 or height < 1 then
				return { ok = false, error = "Resolution requires positive width and height" }
			end
		end

		local density = nil
		if params.pixelDensity ~= nil then
			density = tonumber(params.pixelDensity)
			if density == nil or density <= 0 then
				return { ok = false, error = "Pixel density must be greater than zero" }
			end
		end

		if selectedDevice == nil and orientation == nil and scalingMode == nil and width == nil and density == nil then
			return { ok = false, error = "Set requires a device or configuration option" }
		end

		local changed = {}
		if selectedDevice ~= nil then
			service:SetDeviceAsync(selectedDevice.id)
			changed[#changed + 1] = "device"
		end
		if orientation ~= nil then
			service:SetOrientationAsync(orientation)
			changed[#changed + 1] = "orientation"
		end
		if scalingMode ~= nil then
			service:SetScalingModeAsync(scalingMode)
			changed[#changed + 1] = "scalingMode"
		end
		if width ~= nil then
			service:SetResolutionAsync(math.floor(width), math.floor(height))
			changed[#changed + 1] = "resolution"
		end
		if density ~= nil then
			service:SetPixelDensityAsync(density)
			changed[#changed + 1] = "pixelDensity"
		end

		local wantsPortrait = orientation == Enum.ScreenOrientation.Portrait
		local wantsLandscape = orientation == Enum.ScreenOrientation.LandscapeLeft
			or orientation == Enum.ScreenOrientation.LandscapeRight
			or orientation == Enum.ScreenOrientation.LandscapeSensor
		if wantsPortrait or wantsLandscape then
			local deadline = os.clock() + 2
			while os.clock() < deadline do
				local camera = workspace.CurrentCamera
				local viewport = if camera ~= nil then camera.ViewportSize else Vector2.new(0, 0)
				if (wantsPortrait and viewport.Y >= viewport.X) or (wantsLandscape and viewport.X >= viewport.Y) then
					break
				end
				RunService.Heartbeat:Wait()
			end
		end
		deviceSimulatorReadyAt = os.clock() + 4

		local result = status()
		result.action = "set"
		result.changed = changed
		return result
	end

	function api.executeLuau(params)
		params = params or {}
		local code = tostring(params.code or "")
		if code == "" then
			return { ok = false, error = "Missing Luau code" }
		end
		local timeoutSeconds = math.clamp(tonumber(params.timeoutSeconds) or 10, 0.1, 20)

		local context = string.lower(tostring(params.context or params.target or "plugin"))
		if context == "client" or context == "local" then
			local players = game:GetService("Players")
			local localPlayer = players.LocalPlayer
			if localPlayer == nil then
				return { ok = false, error = "Play client LocalPlayer is not available" }
			end
			local playerScripts = localPlayer:FindFirstChildOfClass("PlayerScripts") or localPlayer:WaitForChild("PlayerScripts", 2)
			if playerScripts == nil then
				return { ok = false, error = "PlayerScripts is not available on the play client" }
			end

			return executeWithRunner(playerScripts, "LocalScript", CLIENT_RUNNER_NAME, nil, code, timeoutSeconds, "client")
		end

		local okRunning, running = pcall(function()
			return RunService:IsRunning()
		end)
		local okServer, isServer = pcall(function()
			return RunService:IsServer()
		end)
		if okRunning and running == true and okServer and isServer == true then
			return executeWithRunner(
				game:GetService("ServerScriptService"),
				"Script",
				SERVER_RUNNER_NAME,
				Enum.RunContext.Server,
				code,
				timeoutSeconds,
				"server"
			)
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

		local packed = nil
		local executionThread = task.spawn(function()
			packed = table.pack(pcall(chunk))
		end)
		local deadline = os.clock() + timeoutSeconds
		while packed == nil and os.clock() < deadline do
			task.wait()
		end
		if packed == nil then
			pcall(task.cancel, executionThread)
			return {
				ok = false,
				error = ("Luau execution timed out after %.1fs and was stopped"):format(timeoutSeconds),
				timedOut = true,
				stopped = true,
				output = output,
			}
		end
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
