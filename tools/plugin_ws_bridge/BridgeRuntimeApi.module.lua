local BridgeRuntimeApi = {}
local BridgeValueCodec = require(script.Parent.BridgeValueCodec)

local CONSOLE_BUFFER_LIMIT = 1000
local CONSOLE_MESSAGE_BYTE_LIMIT = 256 * 1024
local COMMAND_OUTPUT_ENTRY_LIMIT = 256
local COMMAND_OUTPUT_BYTE_LIMIT = 256 * 1024
local COMMAND_OUTPUT_MESSAGE_BYTE_LIMIT = 64 * 1024
local CLIENT_RUNNER_NAME = "__ReniumClientRunner"
local SERVER_RUNNER_NAME = "__ReniumServerRunner"
local MOUSE_PROBE_NAME = "__ReniumMouseProbe"
local runnerSequence = 0

local function truncateText(text, maxBytes)
	if #text <= maxBytes then
		return text
	end
	local suffix = "\n[Renium truncated this message.]"
	local contentBytes = math.max(0, maxBytes - #suffix)
	local ok, boundary = pcall(utf8.offset, text, 0, contentBytes + 1)
	local finish = if ok and boundary then boundary - 1 else contentBytes
	return string.sub(text, 1, finish) .. suffix
end

local function appendCapturedOutput(output, state, kind, ...)
	if state.truncated then
		return
	end
	local values = table.pack(...)
	local parts = table.create(values.n)
	for index = 1, values.n do
		parts[index] = tostring(values[index])
	end
	local message = table.concat(parts, "\t")
	message = truncateText(message, COMMAND_OUTPUT_MESSAGE_BYTE_LIMIT)
	local nextBytes = state.bytes + #message
	if #output >= COMMAND_OUTPUT_ENTRY_LIMIT or nextBytes > COMMAND_OUTPUT_BYTE_LIMIT then
		output[#output + 1] = {
			type = "warn",
			message = "[Renium stopped capturing command output at its size limit.]",
		}
		state.truncated = true
		return
	end
	state.bytes = nextBytes
	output[#output + 1] = { type = kind, message = message }
end

local function consoleTypeName(messageType)
	local text = tostring(messageType or "")
	local dot = string.find(text, "%.[^%.]*$")
	if dot ~= nil then
		return string.sub(text, dot + 1)
	end
	return text
end

local function escapePathSegment(name)
	if name == "" then
		return "\\0"
	end
	return (string.gsub(name, "[\\%.%[%]]", function(character)
		return "\\" .. character
	end))
end

local function unescapePathSegment(text)
	if text == "\\0" then
		return ""
	end
	local out = {}
	local index = 1
	while index <= #text do
		local character = string.sub(text, index, index)
		if character == "\\" and index < #text then
			index += 1
			character = string.sub(text, index, index)
		end
		out[#out + 1] = character
		index += 1
	end
	return table.concat(out)
end

local function splitAutomationPath(text)
	local segments = {}
	local segmentStart = 1
	local index = 1
	while index <= #text do
		local character = string.sub(text, index, index)
		if character == "\\" then
			index += 2
		elseif character == "." then
			segments[#segments + 1] = string.sub(text, segmentStart, index - 1)
			segmentStart = index + 1
			index += 1
		else
			index += 1
		end
	end
	segments[#segments + 1] = string.sub(text, segmentStart)
	return segments
end

local function reverseArray(values)
	for left = 1, math.floor(#values / 2) do
		local right = #values - left + 1
		values[left], values[right] = values[right], values[left]
	end
end

local function compactInstancePath(instance)
	local parts = {}
	local current = instance
	while current ~= nil and current ~= game do
		local parent = current.Parent
		local ordinal = 0
		local duplicateCount = 0
		if parent then
			for _, sibling in ipairs(parent:GetChildren()) do
				if sibling.Name == current.Name then
					duplicateCount += 1
					if sibling == current then
						ordinal = duplicateCount
					end
				end
			end
		end
		local segment = escapePathSegment(current.Name)
		if duplicateCount > 1 then
			segment ..= ("[%d]"):format(ordinal)
		end
		parts[#parts + 1] = segment
		current = current.Parent
	end
	reverseArray(parts)
	return "game." .. table.concat(parts, ".")
end

local function setRunnerSource(runner, code)
	runnerSequence += 1
	runner.Enabled = false
	runner.Source = code
	runner.Enabled = true
end

local function serializeApiValue(value, depth, seen)
	depth = depth or 0
	if depth > 6 then
		return { _type = "Truncated", value = tostring(value) }
	end
	local valueType = typeof(value)
	if value == nil then
		return { _type = "Nil" }
	end
	if valueType == "boolean" or valueType == "string" then
		return value
	end
	if valueType == "number" then
		return BridgeValueCodec.encodeNumber(value)
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
		local components = BridgeValueCodec.encodeComponents(value.X, value.Y)
		return { _type = "Vector2", x = components[1], y = components[2] }
	end
	if valueType == "Vector3" then
		local components = BridgeValueCodec.encodeComponents(value.X, value.Y, value.Z)
		return { _type = "Vector3", x = components[1], y = components[2], z = components[3] }
	end
	if valueType == "Color3" then
		local components = BridgeValueCodec.encodeComponents(value.R, value.G, value.B)
		return { _type = "Color3", r = components[1], g = components[2], b = components[3] }
	end
	if valueType == "CFrame" then
		return { _type = "CFrame", components = BridgeValueCodec.encodeComponents(value:GetComponents()) }
	end
	if valueType == "UDim" then
		local components = BridgeValueCodec.encodeComponents(value.Scale, value.Offset)
		return { _type = "UDim", scale = components[1], offset = components[2] }
	end
	if valueType == "UDim2" then
		local components =
			BridgeValueCodec.encodeComponents(value.X.Scale, value.X.Offset, value.Y.Scale, value.Y.Offset)
		return {
			_type = "UDim2",
			xScale = components[1],
			xOffset = components[2],
			yScale = components[3],
			yOffset = components[4],
		}
	end
	if valueType == "Rect" then
		local components = BridgeValueCodec.encodeComponents(value.Min.X, value.Min.Y, value.Max.X, value.Max.Y)
		return {
			_type = "Rect",
			minX = components[1],
			minY = components[2],
			maxX = components[3],
			maxY = components[4],
		}
	end
	if valueType == "NumberRange" then
		local components = BridgeValueCodec.encodeComponents(value.Min, value.Max)
		return { _type = "NumberRange", min = components[1], max = components[2] }
	end
	if valueType == "BrickColor" then
		return { _type = "BrickColor", number = value.Number }
	end
	if valueType == "PhysicalProperties" then
		local components = BridgeValueCodec.encodeComponents(
			value.Density,
			value.Friction,
			value.Elasticity,
			value.FrictionWeight,
			value.ElasticityWeight
		)
		return {
			_type = "PhysicalProperties",
			density = components[1],
			friction = components[2],
			elasticity = components[3],
			frictionWeight = components[4],
			elasticityWeight = components[5],
			acousticAbsorption = BridgeValueCodec.encodeNumber(value.AcousticAbsorption),
		}
	end
	if valueType == "ColorSequence" then
		local keypoints = table.create(#value.Keypoints)
		for index, keypoint in ipairs(value.Keypoints) do
			local components =
				BridgeValueCodec.encodeComponents(keypoint.Time, keypoint.Value.R, keypoint.Value.G, keypoint.Value.B)
			keypoints[index] = {
				time = components[1],
				color = {
					_type = "Color3",
					r = components[2],
					g = components[3],
					b = components[4],
				},
			}
		end
		return { _type = "ColorSequence", keypoints = keypoints }
	end
	if valueType == "NumberSequence" then
		local keypoints = table.create(#value.Keypoints)
		for index, keypoint in ipairs(value.Keypoints) do
			local components = BridgeValueCodec.encodeComponents(keypoint.Time, keypoint.Value, keypoint.Envelope)
			keypoints[index] = {
				time = components[1],
				value = components[2],
				envelope = components[3],
			}
		end
		return { _type = "NumberSequence", keypoints = keypoints }
	end
	if valueType == "EnumItem" then
		return { _type = "EnumItem", value = tostring(value) }
	end
	if valueType == "table" then
		seen = seen or {}
		if seen[value] then
			return { _type = "Cycle", value = tostring(value) }
		end
		seen[value] = true

		local length = #value
		local count = 0
		local dense = true
		for key in pairs(value) do
			count += 1
			if type(key) ~= "number" or key % 1 ~= 0 or key < 1 or key > length then
				dense = false
			end
		end
		if dense and count == length then
			local out = table.create(length)
			for index = 1, length do
				out[index] = serializeApiValue(value[index], depth + 1, seen)
			end
			seen[value] = nil
			return out
		end

		local entries = {}
		local truncated = false
		count = 0
		for key, nested in pairs(value) do
			count += 1
			if count > 128 then
				truncated = true
				break
			end
			entries[#entries + 1] = {
				key = serializeApiValue(key, depth + 1, seen),
				value = serializeApiValue(nested, depth + 1, seen),
			}
		end
		seen[value] = nil
		local out = { _type = "Table", entries = entries }
		if truncated then
			out.truncated = true
		end
		return out
	end
	return { _type = valueType, value = tostring(value) }
end

local function cancelTrackedThreads(threads)
	for thread in pairs(threads) do
		threads[thread] = nil
		if coroutine.status(thread) ~= "dead" then
			pcall(task.cancel, thread)
		end
	end
end

local function createTrackedTaskProxy(trackedThreads)
	local proxy = table.clone(task)

	local function schedule(scheduleCallback, callback, ...)
		if type(callback) == "thread" then
			local scheduledThread = scheduleCallback(callback, ...)
			if coroutine.status(scheduledThread) ~= "dead" then
				trackedThreads[scheduledThread] = true
			end
			return scheduledThread
		end
		local completed = false
		local scheduledThread = nil
		local arguments = table.pack(...)
		scheduledThread = scheduleCallback(function()
			local result = table.pack(pcall(callback, table.unpack(arguments, 1, arguments.n)))
			completed = true
			if scheduledThread then
				trackedThreads[scheduledThread] = nil
			end
			if not result[1] then
				error(result[2], 0)
			end
			return table.unpack(result, 2, result.n)
		end)
		if not completed then
			trackedThreads[scheduledThread] = true
		end
		return scheduledThread
	end

	proxy.spawn = function(callback, ...)
		return schedule(task.spawn, callback, ...)
	end
	proxy.defer = function(callback, ...)
		return schedule(task.defer, callback, ...)
	end
	proxy.delay = function(duration, callback, ...)
		return schedule(function(worker, ...)
			return task.delay(duration, worker, ...)
		end, callback, ...)
	end
	proxy.cancel = function(thread)
		trackedThreads[thread] = nil
		return task.cancel(thread)
	end
	return proxy
end

local function createTrackedCoroutineProxy(trackedThreads)
	local proxy = table.clone(coroutine)
	local baseCreate = coroutine.create
	local baseResume = coroutine.resume
	local baseClose = coroutine.close

	proxy.create = function(callback)
		local thread = baseCreate(callback)
		trackedThreads[thread] = true
		return thread
	end
	proxy.resume = function(thread, ...)
		local results = table.pack(baseResume(thread, ...))
		if coroutine.status(thread) == "dead" then
			trackedThreads[thread] = nil
		end
		return table.unpack(results, 1, results.n)
	end
	proxy.wrap = function(callback)
		local thread = proxy.create(callback)
		return function(...)
			local results = table.pack(proxy.resume(thread, ...))
			if not results[1] then
				error(results[2], 2)
			end
			return table.unpack(results, 2, results.n)
		end
	end
	proxy.close = function(thread)
		trackedThreads[thread] = nil
		return baseClose(thread)
	end
	return proxy
end

function BridgeRuntimeApi.create(plugin, runtimeContext)
	local HttpService = game:GetService("HttpService")
	local LogService = game:GetService("LogService")
	local RunService = game:GetService("RunService")
	local StudioTestService = game:GetService("StudioTestService")
	local consoleBuffer = table.create(CONSOLE_BUFFER_LIMIT)
	local consoleStart = 1
	local consoleCount = 0
	local consoleSeq = 21335
	local consoleDropped = false
	local consoleEpoch = HttpService:GenerateGUID(false)
	local playSession = {
		token = 0,
		active = false,
		starting = false,
		owned = false,
		ownerGeneration = nil,
		ownerRuntimeId = nil,
		mode = nil,
		lastError = nil,
		lastResult = nil,
		lastStartedAt = 0,
		lastStoppedAt = 0,
		launchNonce = nil,
	}
	local deviceSimulatorReadyAt = os.clock() + 4
	local deviceSimulatorStatusCache = nil
	local captureProbeGui = nil
	local captureProbeFrame = nil
	local activeEditThreads = {}
	local editExecutionToken = 0
	local activeEditExecutionThread = nil
	local cancellationGeneration = 0
	local retainedRunners = {}
	local retainedRunnerSequence = 0

	local function assertOperationOwnership(operationGeneration, sessionGeneration)
		if operationGeneration ~= cancellationGeneration then
			error("Renium operation was cancelled")
		end
		runtimeContext.assertSessionOwnership(sessionGeneration)
	end

	local function appendConsoleEntry(message, messageType)
		consoleSeq += 1
		local text = truncateText(tostring(message), CONSOLE_MESSAGE_BYTE_LIMIT)
		local entry = {
			seq = consoleSeq,
			time = os.clock(),
			unix = os.time(),
			message = text,
			type = consoleTypeName(messageType),
		}
		if consoleCount < CONSOLE_BUFFER_LIMIT then
			local index = ((consoleStart + consoleCount - 1) % CONSOLE_BUFFER_LIMIT) + 1
			consoleBuffer[index] = entry
			consoleCount += 1
		else
			consoleBuffer[consoleStart] = entry
			consoleStart = (consoleStart % CONSOLE_BUFFER_LIMIT) + 1
			consoleDropped = true
		end
	end

	local function consoleEntryAt(position)
		if position < 1 or position > consoleCount then
			return nil
		end
		local index = ((consoleStart + position - 2) % CONSOLE_BUFFER_LIMIT) + 1
		return consoleBuffer[index]
	end

	local historyLoading = true
	local pendingHistoryEntries = {}
	LogService.MessageOut:Connect(function(message, messageType)
		if historyLoading then
			pendingHistoryEntries[#pendingHistoryEntries + 1] = {
				message = message,
				messageType = messageType,
			}
		else
			appendConsoleEntry(message, messageType)
		end
	end)
	local history = LogService:GetLogHistory()
	if type(history) == "table" then
		for _, entry in ipairs(history) do
			if type(entry) == "table" then
				appendConsoleEntry(entry.message or entry.Message or "", entry.messageType or entry.MessageType)
			end
		end
	end
	local overlap = 0
	if type(history) == "table" then
		local limit = math.min(#history, #pendingHistoryEntries)
		for count = limit, 1, -1 do
			local matches = true
			for offset = 1, count do
				local historyEntry = history[#history - count + offset]
				local pendingEntry = pendingHistoryEntries[offset]
				if
					type(historyEntry) ~= "table"
					or tostring(historyEntry.message or historyEntry.Message or "") ~= tostring(pendingEntry.message)
					or consoleTypeName(historyEntry.messageType or historyEntry.MessageType)
						~= consoleTypeName(pendingEntry.messageType)
				then
					matches = false
					break
				end
			end
			if matches then
				overlap = count
				break
			end
		end
	end
	for index = overlap + 1, #pendingHistoryEntries do
		local entry = pendingHistoryEntries[index]
		appendConsoleEntry(entry.message, entry.messageType)
	end
	historyLoading = false

	local api = {}

	local function executeWithRunner(
		parent,
		className,
		baseName,
		runContext,
		code,
		timeoutSeconds,
		context,
		backgroundLifetimeSeconds,
		operationGeneration
	)
		assertOperationOwnership(operationGeneration)
		local scriptInstance = Instance.new(className)
		scriptInstance.Name = baseName .. "_" .. tostring(runnerSequence + 1)
		scriptInstance.Enabled = false
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
			if
				logError == nil
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
local outputBytes = 0
local outputTruncated = false
local function truncate(text, maxBytes)
	if #text <= maxBytes then return text end
	local suffix = "\n[Renium truncated this message.]"
	local contentBytes = math.max(0, maxBytes - #suffix)
	local ok, boundary = pcall(utf8.offset, text, 0, contentBytes + 1)
	local finish = if ok and boundary then boundary - 1 else contentBytes
	return string.sub(text, 1, finish) .. suffix
end
local function capture(kind, ...)
	local values = table.pack(...)
	local parts = table.create(values.n)
	for index = 1, values.n do parts[index] = tostring(values[index]) end
	if not outputTruncated then
		local message = truncate(table.concat(parts, "\t"), %d)
		if #output >= %d or outputBytes + #message > %d then
			output[#output + 1] = { type = "warn", message = "[Renium stopped capturing command output at its size limit.]" }
			outputTruncated = true
		else
			outputBytes += #message
			output[#output + 1] = { type = kind, message = message }
		end
	end
end
local print = function(...) capture("print", ...) end
local warn = function(...) capture("warn", ...) end
local finished = false
local worker
worker = task.spawn(function()
	local packed = table.pack(xpcall(function()
%s
	end, function(message)
		return debug.traceback(tostring(message), 2)
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
]==]):format(
			COMMAND_OUTPUT_MESSAGE_BYTE_LIMIT,
			COMMAND_OUTPUT_ENTRY_LIMIT,
			COMMAND_OUTPUT_BYTE_LIMIT,
			code,
			timeoutSeconds
		)
		setRunnerSource(scriptInstance, wrapped)
		local deadline = os.clock() + timeoutSeconds + 1
		while status == nil and logError == nil and scriptInstance.Parent ~= nil and os.clock() < deadline do
			task.wait()
			local ownsSession, ownershipError = pcall(assertOperationOwnership, operationGeneration)
			if not ownsSession then
				logError = tostring(ownershipError)
			end
		end
		resultConnection:Disconnect()
		logConnection:Disconnect()
		local retained = status == "ok" and backgroundLifetimeSeconds and backgroundLifetimeSeconds > 0
		local executionId = nil
		if retained then
			retainedRunnerSequence += 1
			executionId = `{runtimeContext.runtimeId or "runtime"}:{retainedRunnerSequence}`
			retainedRunners[executionId] = {
				instance = scriptInstance,
				generation = operationGeneration,
			}
			task.delay(backgroundLifetimeSeconds, function()
				local entry = retainedRunners[executionId]
				if entry ~= nil and entry.instance == scriptInstance then
					retainedRunners[executionId] = nil
					scriptInstance:Destroy()
				end
			end)
		else
			scriptInstance.Enabled = false
			scriptInstance:Destroy()
		end
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
			background = not not retained,
			executionId = executionId,
			path = path,
			context = context,
		}
	end

	local function currentRunStateText()
		return tostring(RunService.RunState)
	end

	local function currentStudioTestState()
		local editModeOk, editModeValue = pcall(function()
			return (StudioTestService :: any).EditModeActive
		end)
		local canLeaveOk, canLeaveValue = pcall((StudioTestService :: any).CanLeaveTest, StudioTestService)
		return {
			editModeActive = if editModeOk and type(editModeValue) == "boolean" then editModeValue else nil,
			canLeaveTest = if canLeaveOk and type(canLeaveValue) == "boolean" then canLeaveValue else nil,
		}
	end

	local function getInstanceDebugId(instance)
		local debugId = instance:GetDebugId(32)
		return if type(debugId) == "string" then debugId else nil
	end

	local function parseGuiSegment(text)
		local name, ordinalText = string.match(text, "^(.-)%[(%d+)%]$")
		if name ~= nil and name ~= "" then
			return unescapePathSegment(name), tonumber(ordinalText)
		end
		return unescapePathSegment(text), nil
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
				if not current.Visible then
					return false
				end
			elseif current:IsA("ScreenGui") then
				return current.Enabled
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
			if current:IsA("GuiObject") and (current.ClipsDescendants or current:IsA("ScrollingFrame")) then
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
						current.CanvasPosition = Vector2.new(math.clamp(targetX, 0, maxX), math.clamp(targetY, 0, maxY))
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
			local segment = escapePathSegment(current.Name)
			if #siblings > 1 then
				for position, sibling in ipairs(siblings) do
					if sibling == current then
						segment ..= ("[%d]"):format(position)
						break
					end
				end
			end
			parts[#parts + 1] = segment
			current = parent
		end
		reverseArray(parts)
		return table.concat(parts, ".")
	end

	local function guiBoundsResult(current, root, matchedCount)
		local absPos = current.AbsolutePosition
		local absSize = current.AbsoluteSize
		local centerX = absPos.X + absSize.X / 2
		local centerY = absPos.Y + absSize.Y / 2
		local hitTest = false
		local blockedBy
		local playerGui = current:FindFirstAncestorOfClass("PlayerGui")
		if playerGui then
			for _, hit in ipairs(playerGui:GetGuiObjectsAtPosition(centerX, centerY)) do
				if hit == current or hit:IsDescendantOf(current) then
					hitTest = true
					break
				elseif
					not current:IsDescendantOf(hit)
					and (hit:IsA("GuiButton") or hit:IsA("TextBox") or hit.Active)
				then
					blockedBy = hit:GetFullName()
					break
				end
			end
		end
		local insetX, insetY = 0, 0
		local topLeft = (game:GetService("GuiService") :: any):GetGuiInset()
		if typeof(topLeft) == "Vector2" then
			insetX, insetY = topLeft.X, topLeft.Y
		end
		local camera = game:GetService("Workspace").CurrentCamera
		local viewport = if camera ~= nil then camera.ViewportSize else Vector2.new(0, 0)
		return {
			ok = true,
			x = centerX + insetX,
			y = centerY + insetY,
			left = absPos.X + insetX,
			top = absPos.Y + insetY,
			width = absSize.X,
			height = absSize.Y,
			visible = isEffectivelyVisible(current),
			onScreen = guiCenterOnScreen(current, insetX, insetY),
			viewportWidth = viewport.X,
			viewportHeight = viewport.Y,
			className = current.ClassName,
			fullName = current:GetFullName(),
			ordinalPath = guiOrdinalPath(root, current),
			id = getInstanceDebugId(current),
			matchedCount = matchedCount,
			hitTest = hitTest,
			blockedBy = blockedBy,
		}
	end

	function api.getGuiBounds(params)
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
		local segments = splitAutomationPath(pathText)
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
					error = ("Path segment '%s' matched nothing under %s"):format(segments[index], pathText),
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
		local topLeft = (game:GetService("GuiService") :: any):GetGuiInset()
		if typeof(topLeft) == "Vector2" then
			insetX = topLeft.X
			insetY = topLeft.Y
		end
		local visibilityByInstance = { [playerGui] = true }
		local screenGuiByInstance = { [playerGui] = false }
		local pathByInstance = { [playerGui] = "" }
		local ordinalSegmentByInstance = {}
		local ordinalReadyByParent = {}
		local clipByInstance = {}
		local clipReadyByInstance = {}

		local function inventoryVisibility(instance)
			local cached = visibilityByInstance[instance]
			if cached ~= nil then
				local screenGui = screenGuiByInstance[instance]
				return cached, if screenGui then screenGui else nil
			end
			local parent = instance.Parent
			if not parent then
				return false, nil
			end
			local parentVisible, screenGui = inventoryVisibility(parent)
			local visible = parentVisible
			if instance:IsA("ScreenGui") then
				screenGui = instance
				visible = instance.Enabled
			elseif instance:IsA("GuiObject") then
				visible = parentVisible and screenGui ~= nil and instance.Visible
			end
			visibilityByInstance[instance] = visible
			screenGuiByInstance[instance] = screenGui or false
			return visible, screenGui
		end

		local function ensureOrdinalSegments(parent)
			if ordinalReadyByParent[parent] then
				return
			end
			ordinalReadyByParent[parent] = true
			local children = parent:GetChildren()
			local totals = {}
			for _, child in ipairs(children) do
				totals[child.Name] = (totals[child.Name] or 0) + 1
			end
			local positions = {}
			for _, child in ipairs(children) do
				local name = child.Name
				positions[name] = (positions[name] or 0) + 1
				local escapedName = escapePathSegment(name)
				ordinalSegmentByInstance[child] = if totals[name] > 1
					then ("%s[%d]"):format(escapedName, positions[name])
					else escapedName
			end
		end

		local function inventoryPath(instance)
			local cached = pathByInstance[instance]
			if cached then
				return cached
			end
			local parent = instance.Parent
			if not parent then
				return instance.Name
			end
			ensureOrdinalSegments(parent)
			local parentPath = inventoryPath(parent)
			local segment = ordinalSegmentByInstance[instance] or instance.Name
			local path = if parentPath == "" then segment else parentPath .. "." .. segment
			pathByInstance[instance] = path
			return path
		end

		local function inheritedClip(instance)
			if clipReadyByInstance[instance] then
				local cached = clipByInstance[instance]
				return if cached then cached else nil
			end
			clipReadyByInstance[instance] = true
			local parent = instance.Parent
			local clip = if parent then inheritedClip(parent) else nil
			if parent and parent:IsA("GuiObject") and (parent.ClipsDescendants or parent:IsA("ScrollingFrame")) then
				local position = parent.AbsolutePosition
				local size = parent.AbsoluteSize
				local nextClip = {
					left = position.X,
					top = position.Y,
					right = position.X + size.X,
					bottom = position.Y + size.Y,
				}
				if clip then
					nextClip.left = math.max(nextClip.left, clip.left)
					nextClip.top = math.max(nextClip.top, clip.top)
					nextClip.right = math.min(nextClip.right, clip.right)
					nextClip.bottom = math.min(nextClip.bottom, clip.bottom)
				end
				clip = nextClip
			end
			clipByInstance[instance] = clip or false
			return clip
		end

		local function inventoryOnScreen(instance, offX, offY)
			local position = instance.AbsolutePosition
			local size = instance.AbsoluteSize
			local centerX = position.X + size.X / 2
			local centerY = position.Y + size.Y / 2
			local camera = workspace.CurrentCamera
			if camera then
				local viewport = camera.ViewportSize
				local viewportX = centerX + offX
				local viewportY = centerY + offY
				if viewportX < 0 or viewportY < 0 or viewportX > viewport.X or viewportY > viewport.Y then
					return false
				end
			end
			local clip = inheritedClip(instance)
			return not clip
				or (centerX >= clip.left and centerX <= clip.right and centerY >= clip.top and centerY <= clip.bottom)
		end

		local items = {}
		local truncated = false
		local includeOffscreen = params.includeOffscreen == true
		for _, descendant in ipairs(playerGui:GetDescendants()) do
			if descendant:IsA("GuiButton") or descendant:IsA("TextBox") then
				local visible = inventoryVisibility(descendant)
				if not visible then
					continue
				end
				local absSize = descendant.AbsoluteSize
				if absSize.X <= 0 or absSize.Y <= 0 then
					continue
				end
				local offX, offY = insetX, insetY
				if not includeOffscreen and not inventoryOnScreen(descendant, offX, offY) then
					continue
				end
				if #items >= limit then
					truncated = true
					break
				end
				local absPos = descendant.AbsolutePosition
				local text = if descendant:IsA("TextButton") or descendant:IsA("TextBox")
					then string.sub(descendant.Text, 1, 60)
					else nil
				items[#items + 1] = {
					p = inventoryPath(descendant),
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
		local pathText = tostring(params.path or "")
		if pathText == "" then
			return { ok = false, error = "Missing world instance path" }
		end
		local segments = splitAutomationPath(pathText)
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
		local onScreen = inFront and point.X >= 0 and point.Y >= 0 and point.X <= viewport.X and point.Y <= viewport.Y
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
		if probe and probe:GetAttribute("ProbeVersion") ~= 3 then
			probe:Destroy()
			probe = nil
		end
		if probe == nil then
			probe = Instance.new("LocalScript")
			probe.Name = MOUSE_PROBE_NAME
			setRunnerSource(
				probe,
				[==[
local probe = script
local UserInputService = game:GetService("UserInputService")
probe:SetAttribute("ProbeVersion", 3)
local function updateMouse()
	local location = UserInputService:GetMouseLocation()
	probe:SetAttribute("MouseX", location.X)
	probe:SetAttribute("MouseY", location.Y)
	probe:SetAttribute("ResponseSeq", probe:GetAttribute("RequestSeq") or 0)
end
probe:GetAttributeChangedSignal("RequestSeq"):Connect(updateMouse)
updateMouse()
]==]
			)
			probe.Parent = playerScripts
		end
		local requestSeq = (probe:GetAttribute("RequestSeq") or 0) + 1
		probe:SetAttribute("RequestSeq", requestSeq)
		local deadline = os.clock() + 1
		while probe.Parent and probe:GetAttribute("ResponseSeq") ~= requestSeq and os.clock() < deadline do
			task.wait()
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
			viewportWidth = viewport.X,
			viewportHeight = viewport.Y,
		}
	end

	function api.sendVirtualInput(params)
		if not RunService:IsRunning() or not RunService:IsClient() then
			return { ok = false, error = "Virtual input requires a Play client" }
		end
		local actions = params.actions
		if type(actions) ~= "table" or #actions < 1 or #actions > 1024 then
			return { ok = false, error = "Virtual input requires 1 through 1024 actions" }
		end
		local function findGuiButtonById(id)
			local localPlayer = game:GetService("Players").LocalPlayer
			local playerGui = if localPlayer then localPlayer:FindFirstChildOfClass("PlayerGui") else nil
			if playerGui then
				for _, descendant in ipairs(playerGui:GetDescendants()) do
					if descendant:IsA("GuiButton") and getInstanceDebugId(descendant) == id then
						return descendant
					end
				end
			end
			return nil
		end

		local activationId = tostring(params.expectActivatedId or "")
		local activationCount = 0
		local clickCount = 0
		local activationConnection
		local clickConnection
		if activationId ~= "" then
			local target = findGuiButtonById(activationId)
			if target == nil then
				return { ok = false, error = "The expected GuiButton is no longer available" }
			end
			activationConnection = target.Activated:Connect(function()
				activationCount += 1
			end)
			clickConnection = target.MouseButton1Click:Connect(function()
				clickCount += 1
			end)
		end

		local virtualInput = game:GetService("UserInputService"):CreateVirtualInput()
		local verifiedClicks = 0
		local function sendVerifiedClick(action)
			local bounds = api.getGuiBounds({ path = action.path, id = action.id, scroll = true })
			if bounds.ok ~= true then
				error(tostring(bounds.error or "GUI target could not be resolved"), 0)
			end
			if bounds.visible ~= true then
				error(`GUI element {tostring(bounds.fullName)} is not visible`, 0)
			end
			if bounds.onScreen ~= true then
				error(`GUI element {tostring(bounds.fullName)} is outside the viewport`, 0)
			end
			if bounds.hitTest ~= true then
				local blocker = tostring(bounds.blockedBy or "another GUI element")
				error(`GUI element {tostring(bounds.fullName)} is covered by {blocker}`, 0)
			end
			local target = findGuiButtonById(tostring(bounds.id or ""))
			if target == nil then
				error("The expected GuiButton is no longer available", 0)
			end
			local activated = 0
			local clicked = 0
			local activatedConnection = target.Activated:Connect(function()
				activated += 1
			end)
			local clickedConnection = target.MouseButton1Click:Connect(function()
				clicked += 1
			end)
			local position = Vector2.new(tonumber(bounds.x) or 0, tonumber(bounds.y) or 0)
			virtualInput:SendMouseButton(position, Enum.UserInputType.MouseButton1, true, 0)
			task.wait(math.clamp((tonumber(action.holdMs) or 30) / 1000, 0, 10))
			virtualInput:SendMouseButton(position, Enum.UserInputType.MouseButton1, false, 0)
			local deadline = os.clock() + 1
			while (activated == 0 or clicked == 0) and os.clock() < deadline do
				RunService.Heartbeat:Wait()
			end
			activatedConnection:Disconnect()
			clickedConnection:Disconnect()
			if activated ~= 1 or clicked ~= 1 then
				error(`Expected one Activated and MouseButton1Click event, got {activated} and {clicked}`, 0)
			end
			verifiedClicks += 1
		end
		for _, action in actions do
			local actionType = tostring(action.type or "")
			if actionType == "wait" then
				task.wait(math.clamp((tonumber(action.ms) or 0) / 1000, 0, 10))
			elseif actionType == "key" then
				local keyCode = Enum.KeyCode[tostring(action.key or "")]
				if keyCode == nil then
					error("Unknown key " .. tostring(action.key), 0)
				end
				virtualInput:SendKey(action.down == true, keyCode, action.repeated == true)
			elseif actionType == "text" then
				virtualInput:SendTextInput(tostring(action.text or ""))
			elseif actionType == "click" then
				sendVerifiedClick(action)
			else
				local position = Vector2.new(tonumber(action.x) or 0, tonumber(action.y) or 0)
				if actionType == "move" then
					virtualInput:SendMousePosition(position)
				elseif actionType == "button" then
					local button = Enum.UserInputType.MouseButton1
					if action.button == "right" then
						button = Enum.UserInputType.MouseButton2
					elseif action.button == "middle" then
						button = Enum.UserInputType.MouseButton3
					end
					virtualInput:SendMouseButton(position, button, action.down == true, tonumber(action.repeatCount) or 0)
				elseif actionType == "scroll" then
					virtualInput:SendPointerAction(position, { Wheel = tonumber(action.delta) or 0 })
				else
					error("Unknown virtual input action " .. actionType, 0)
				end
			end
		end
		if activationConnection then
			local deadline = os.clock() + 1
			while (activationCount == 0 or clickCount == 0) and os.clock() < deadline do
				RunService.Heartbeat:Wait()
			end
			activationConnection:Disconnect()
			clickConnection:Disconnect()
			if activationCount ~= 1 or clickCount ~= 1 then
				return {
					ok = false,
					error = `Expected one Activated and MouseButton1Click event, got {activationCount} and {clickCount}`,
				}
			end
		else
			task.wait()
		end
		return {
			ok = true,
			actions = #actions,
			verifiedClicks = verifiedClicks,
			activated = if activationId ~= "" then activationCount == 1 else nil,
			clicked = if activationId ~= "" then clickCount == 1 else nil,
		}
	end

	function api.getConsoleOutput(params)
		local limit = if params.clear == true
			then CONSOLE_BUFFER_LIMIT
			else math.clamp(tonumber(params.limit) or 200, 1, CONSOLE_BUFFER_LIMIT)
		local sinceSeq = tonumber(params.sinceSeq) or tonumber(params.since) or tonumber(params.cursorSeq) or 0
		local entries = {}
		local truncated = false
		local hasMore = false
		local nextSeq = consoleSeq

		if params.cursorOnly ~= true then
			if sinceSeq > 0 then
				local oldest = consoleEntryAt(1)
				truncated = oldest ~= nil and sinceSeq < oldest.seq - 1
				for position = 1, consoleCount do
					local entry = consoleEntryAt(position)
					if entry and entry.seq > sinceSeq then
						if #entries >= limit then
							hasMore = true
							break
						end
						entries[#entries + 1] = entry
					end
				end
				if #entries > 0 then
					nextSeq = entries[#entries].seq
				end
			else
				local startPosition = if params.fromOldest == true then 1 else math.max(1, consoleCount - limit + 1)
				truncated = consoleDropped or params.fromOldest ~= true and startPosition > 1
				for position = startPosition, consoleCount do
					if #entries >= limit then
						hasMore = true
						break
					end
					entries[#entries + 1] = consoleEntryAt(position)
				end
				if #entries > 0 then
					nextSeq = entries[#entries].seq
				end
			end
		end
		if params.clear == true then
			table.clear(consoleBuffer)
			consoleStart = 1
			consoleCount = 0
			consoleDropped = false
		end
		return {
			ok = true,
			entries = entries,
			count = #entries,
			nextSeq = nextSeq,
			truncated = truncated,
			hasMore = hasMore,
			epoch = consoleEpoch,
		}
	end

	function api.finalConsoleSnapshot()
		return api.getConsoleOutput({
			limit = CONSOLE_BUFFER_LIMIT,
			fromOldest = true,
			clear = false,
		})
	end

	function api.captureViewportProbe(params)
		local operationGeneration = cancellationGeneration
		assertOperationOwnership(operationGeneration)
		local action = string.lower(tostring(params.action or "start"))
		if action == "stop" or action == "clear" then
			if captureProbeGui ~= nil then
				captureProbeGui:Destroy()
			end
			captureProbeGui = nil
			captureProbeFrame = nil
			task.wait()
			assertOperationOwnership(operationGeneration)
			return { ok = true, action = "stop" }
		end
		local colors = params.colors
		if type(colors) ~= "table" or #colors ~= 16 then
			return { ok = false, error = "Capture probe requires 16 colors" }
		end
		local packedColors = table.create(16)
		for index = 1, 16 do
			local packed = tonumber(colors[index])
			if not packed or packed % 1 ~= 0 or packed < 0 or packed > 0xFFFFFF then
				return { ok = false, error = ("Capture probe color %d must be a 24-bit integer"):format(index) }
			end
			packedColors[index] = packed
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
			screen.ScreenInsets = Enum.ScreenInsets.None
			screen.ClipToDeviceSafeArea = false
			screen.SafeAreaCompatibility = Enum.SafeAreaCompatibility.None
			local frame = Instance.new("Frame")
			frame.Name = "Probe"
			frame.Archivable = false
			frame.BackgroundTransparency = 1
			frame.BorderSizePixel = 0
			frame.ClipsDescendants = true
			frame.Size = UDim2.fromScale(1, 1)
			frame.ZIndex = 2147483647
			for index = 1, 16 do
				local packed = packedColors[index]
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
				local packed = packedColors[index]
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
			return { ok = false, error = `Unknown capture probe action '{action}'` }
		end
		task.wait()
		assertOperationOwnership(operationGeneration)
		return { ok = true, action = action }
	end

	function api.deviceSimulator(params)
		local operationGeneration = cancellationGeneration
		assertOperationOwnership(operationGeneration)
		local action = string.lower(tostring(params.action or "status"))
		local service = game:GetService("StudioDeviceSimulatorService")
		if action ~= "list" and action ~= "capture-status" and api.isPlayModeRunning() then
			return { ok = false, error = "Stop Play before reading or changing device simulation" }
		end
		if params.waitForStartup == true then
			while os.clock() < deviceSimulatorReadyAt do
				RunService.Heartbeat:Wait()
				assertOperationOwnership(operationGeneration)
			end
		end

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
				custom = not not info.IsCustom,
				nativeWidth = tonumber(info.Width),
				nativeHeight = tonumber(info.Height),
				nativeResolutionOrientation = if info.Width >= info.Height then "Landscape" else "Portrait",
				nativePixelDensity = tonumber(info.PixelDensity),
				resolutionScale = tonumber(info.ResolutionScale),
				portraitKeyboardHeight = tonumber(info.PortraitKeyboardHeight),
				landscapeKeyboardHeight = tonumber(info.LandscapeKeyboardHeight),
			}
		end

		local function selectionInfo(info)
			return {
				id = info.id,
				name = info.name,
				form = info.form,
				custom = info.custom,
			}
		end

		local function vectorSize(value)
			return { width = math.round(value.X), height = math.round(value.Y) }
		end

		local function status()
			local function remember(result)
				deviceSimulatorStatusCache = table.clone(result)
				return result
			end
			local camera = workspace.CurrentCamera
			local viewport = if camera ~= nil then camera.ViewportSize else Vector2.new(0, 0)
			local inactive = {
				ok = true,
				action = "status",
				playRunning = false,
				simulating = false,
				viewport = vectorSize(viewport),
			}
			local okDevice, deviceId = pcall(service.GetDeviceAsync, service)
			if not okDevice then
				return { ok = false, error = tostring(deviceId) }
			end
			if type(deviceId) ~= "string" or deviceId == "" or deviceId == "default" then
				return remember(inactive)
			end
			local result = {
				ok = true,
				action = "status",
				playRunning = false,
				simulating = true,
				viewport = vectorSize(viewport),
			}
			local okConfig, resolution, orientation, scalingMode, pixelDensity = pcall(function()
				return service:GetResolutionAsync(),
					service:GetOrientationAsync(),
					service:GetScalingModeAsync(),
					service:GetPixelDensityAsync()
			end)
			if okConfig then
				local orientationName = enumName(orientation)
				local portrait = orientationName == "Portrait"
				result.orientation = orientationName
				result.scalingMode = enumName(scalingMode)
				result.resolution = {
					width = if portrait then resolution.Y else resolution.X,
					height = if portrait then resolution.X else resolution.Y,
				}
				result.effectivePixelDensity = pixelDensity
			end
			if params.includeSettle == true then
				result.settleSeconds = math.max(0, deviceSimulatorReadyAt - os.clock())
			end
			local okInfo, info = pcall(deviceInfo, deviceId)
			if okInfo then
				result.device = if params.details == true then info else selectionInfo(info)
			else
				result.device = { id = deviceId, name = deviceId }
			end
			return remember(result)
		end

		if action == "capture-status" then
			if not api.isPlayModeRunning() then
				return status()
			end
			local result = if deviceSimulatorStatusCache then table.clone(deviceSimulatorStatusCache) else {
				ok = true,
				action = "status",
				simulating = false,
			}
			if params.includeSettle == true and result.simulating then
				result.settleSeconds = math.max(0, deviceSimulatorReadyAt - os.clock())
			end
			result.playRunning = true
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
				if
					string.find(normalized(device.id), key, 1, true)
					or string.find(normalized(device.name), key, 1, true)
				then
					matches[#matches + 1] = device
				end
			end
			if #matches == 1 then
				return matches[1]
			end
			if #matches == 0 then
				return nil, `Unknown device '{requested}'`
			end
			local names = {}
			for _, device in ipairs(matches) do
				names[#names + 1] = `{device.name} ({device.id})`
			end
			return nil, `Device '{requested}' is ambiguous: {table.concat(names, ", ")}`
		end

		if action == "list" then
			local devices = listDevices()
			if params.details ~= true then
				for index, device in ipairs(devices) do
					devices[index] = selectionInfo(device)
				end
			end
			return { ok = true, action = "list", count = #devices, devices = devices }
		elseif action == "status" or action == "get" then
			return status()
		elseif action == "stop" or action == "reset" then
			local before = status()
			if not before.ok then
				return before
			end
			local okStop, stopError = pcall(service.StopSimulationAsync, service)
			if not okStop and before.simulating then
				return { ok = false, error = tostring(stopError) }
			end
			local after = status()
			local deadline = os.clock() + 1
			while after.ok and after.simulating and os.clock() < deadline do
				RunService.Heartbeat:Wait()
				assertOperationOwnership(operationGeneration)
				after = status()
			end
			if not after.ok then
				return after
			end
			if after.simulating then
				return { ok = false, error = "Studio did not stop device simulation" }
			end
			deviceSimulatorReadyAt = 0
			return { ok = true, action = "stop", stopped = true, alreadyStopped = not before.simulating }
		elseif action ~= "set" and action ~= "select" and action ~= "apply" then
			return { ok = false, error = `Unknown device action '{action}'` }
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
				return { ok = false, error = `Unknown orientation '{params.orientation}'` }
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
				return { ok = false, error = `Unknown scaling mode '{params.scalingMode}'` }
			end
		end

		local width = tonumber(params.width)
		local height = tonumber(params.height)
		if width or height then
			if not width or not height or width < 1 or height < 1 then
				return { ok = false, error = "Resolution requires positive width and height" }
			end
		end

		local density = nil
		if params.pixelDensity ~= nil then
			density = tonumber(params.pixelDensity)
			if not density or density <= 0 then
				return { ok = false, error = "Pixel density must be greater than zero" }
			end
		end

		if not selectedDevice and not orientation and not scalingMode and not width and not density then
			return { ok = false, error = "Set requires a device or configuration option" }
		end

		local simulationBefore = status()
		local readyAtBefore = deviceSimulatorReadyAt
		local configurationBefore = nil
		if simulationBefore.simulating then
			local okConfig, deviceId, resolution, previousOrientation, previousScalingMode, previousDensity = pcall(
				function()
					return service:GetDeviceAsync(),
						service:GetResolutionAsync(),
						service:GetOrientationAsync(),
						service:GetScalingModeAsync(),
						service:GetPixelDensityAsync()
				end
			)
			if not okConfig then
				return { ok = false, error = tostring(deviceId) }
			end
			configurationBefore = {
				deviceId = deviceId,
				resolution = resolution,
				orientation = previousOrientation,
				scalingMode = previousScalingMode,
				pixelDensity = previousDensity,
			}
		end
		local changed = {}
		local okApply, resultOrError = xpcall(function()
			if selectedDevice ~= nil then
				service:SetDeviceAsync(selectedDevice.id)
				assertOperationOwnership(operationGeneration)
				changed[#changed + 1] = "device"
			end
			if orientation ~= nil then
				service:SetOrientationAsync(orientation)
				assertOperationOwnership(operationGeneration)
				changed[#changed + 1] = "orientation"
			end
			if scalingMode ~= nil then
				service:SetScalingModeAsync(scalingMode)
				assertOperationOwnership(operationGeneration)
				changed[#changed + 1] = "scalingMode"
			end
			if width then
				service:SetResolutionAsync(math.floor(width), math.floor(height))
				assertOperationOwnership(operationGeneration)
				changed[#changed + 1] = "resolution"
			end
			if density then
				service:SetPixelDensityAsync(density)
				assertOperationOwnership(operationGeneration)
				changed[#changed + 1] = "pixelDensity"
			end

			local wantsPortrait = orientation == Enum.ScreenOrientation.Portrait
			local wantsLandscape = orientation == Enum.ScreenOrientation.LandscapeLeft
				or orientation == Enum.ScreenOrientation.LandscapeRight
				or orientation == Enum.ScreenOrientation.LandscapeSensor
			if wantsPortrait or wantsLandscape then
				local deadline = os.clock() + 1
				while os.clock() < deadline do
					local camera = workspace.CurrentCamera
					local viewport = if camera ~= nil then camera.ViewportSize else Vector2.new(0, 0)
					if
						(wantsPortrait and viewport.Y >= viewport.X) or (wantsLandscape and viewport.X >= viewport.Y)
					then
						break
					end
					RunService.Heartbeat:Wait()
					assertOperationOwnership(operationGeneration)
				end
			end
			deviceSimulatorReadyAt = os.clock() + 4

			local result = status()
			assertOperationOwnership(operationGeneration)
			result.action = "set"
			result.changed = changed
			return result
		end, debug.traceback)
		if okApply then
			return resultOrError
		end

		local okRestore, restoreError = pcall(function()
			if configurationBefore == nil then
				if selectedDevice ~= nil then
					service:StopSimulationAsync()
				end
			else
				service:SetDeviceAsync(configurationBefore.deviceId)
				service:SetOrientationAsync(configurationBefore.orientation)
				service:SetScalingModeAsync(configurationBefore.scalingMode)
				service:SetResolutionAsync(configurationBefore.resolution.X, configurationBefore.resolution.Y)
				service:SetPixelDensityAsync(configurationBefore.pixelDensity)
			end
		end)
		deviceSimulatorReadyAt = readyAtBefore
		status()
		if not okRestore then
			error(tostring(resultOrError) .. "; device simulator rollback failed: " .. tostring(restoreError), 0)
		end
		error(resultOrError, 0)
	end

	function api.executeLuau(params)
		local operationGeneration = cancellationGeneration
		assertOperationOwnership(operationGeneration)
		local code = tostring(params.code or "")
		if code == "" then
			return { ok = false, error = "Missing Luau code" }
		end
		local timeoutSeconds = math.clamp(tonumber(params.timeoutSeconds) or 10, 0.1, 120)
		local backgroundLifetimeSeconds = BridgeValueCodec.decodeNumber(params.backgroundLifetimeSeconds)
		backgroundLifetimeSeconds = if backgroundLifetimeSeconds
			then math.clamp(backgroundLifetimeSeconds, 0.1, 610)
			else nil

		local context = string.lower(tostring(params.context or "plugin"))
		if context == "client" then
			local players = game:GetService("Players")
			local localPlayer = players.LocalPlayer
			if localPlayer == nil then
				return { ok = false, error = "Play client LocalPlayer is not available" }
			end
			local playerScripts = localPlayer:FindFirstChildOfClass("PlayerScripts")
				or localPlayer:WaitForChild("PlayerScripts", 2)
			if playerScripts == nil then
				return { ok = false, error = "PlayerScripts is not available on the play client" }
			end

			return executeWithRunner(
				playerScripts,
				"LocalScript",
				CLIENT_RUNNER_NAME,
				nil,
				code,
				timeoutSeconds,
				"client",
				backgroundLifetimeSeconds,
				operationGeneration
			)
		end

		if RunService:IsRunning() and RunService:IsServer() then
			return executeWithRunner(
				game:GetService("ServerScriptService"),
				"Script",
				SERVER_RUNNER_NAME,
				Enum.RunContext.Server,
				code,
				timeoutSeconds,
				"server",
				backgroundLifetimeSeconds,
				operationGeneration
			)
		end

		if type(loadstring) ~= "function" then
			return { ok = false, error = "loadstring is not available in this Studio session" }
		end

		local requestedChunkName = tostring(params.chunkName or "Renium")
		local chunkName = if requestedChunkName == "" then "Renium" else requestedChunkName

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
		local outputState = { bytes = 0, truncated = false }
		local function capture(kind, ...)
			appendCapturedOutput(output, outputState, kind, ...)
		end

		local baseEnv = getfenv(0)
		cancelTrackedThreads(activeEditThreads)
		activeEditThreads = {}
		editExecutionToken += 1
		local executionToken = editExecutionToken
		local trackedThreads = {}
		local trackedTask = createTrackedTaskProxy(trackedThreads)
		local trackedCoroutine = createTrackedCoroutineProxy(trackedThreads)
		local baseGetfenv = getfenv
		local baseSetfenv = setfenv
		local env = setmetatable({}, { __index = baseEnv })
		env.plugin = plugin
		env.task = trackedTask
		env.coroutine = trackedCoroutine
		env.spawn = trackedTask.spawn
		env.delay = trackedTask.delay
		env.print = function(...)
			capture("print", ...)
		end
		env.warn = function(...)
			capture("warn", ...)
		end
		env._G = env
		env.getfenv = function(target)
			local resolved = baseGetfenv(target)
			return if resolved == baseEnv then env else resolved
		end
		env.setfenv = function(target, replacement)
			if replacement == baseEnv then
				error("The Studio global environment cannot be assigned to Renium automation", 2)
			end
			return baseSetfenv(target, replacement)
		end
		setfenv(chunk, env)

		local packed = nil
		local finishedAt = nil
		local deadline = os.clock() + timeoutSeconds
		local executionThread = task.defer(function()
			packed = table.pack(xpcall(chunk, function(message)
				return debug.traceback(tostring(message), 2)
			end))
			finishedAt = os.clock()
		end)
		activeEditExecutionThread = executionThread
		while packed == nil and os.clock() < deadline do
			task.wait()
			if operationGeneration ~= cancellationGeneration then
				break
			end
		end
		if packed == nil or (finishedAt and finishedAt > deadline) or operationGeneration ~= cancellationGeneration then
			if coroutine.status(executionThread) ~= "dead" then
				pcall(task.cancel, executionThread)
			end
			activeEditExecutionThread = nil
			cancelTrackedThreads(trackedThreads)
			return {
				ok = false,
				error = if operationGeneration ~= cancellationGeneration
					then "Luau execution was cancelled because Renium session ownership changed"
					else ("Luau execution timed out after %.1fs and was stopped"):format(timeoutSeconds),
				timedOut = operationGeneration == cancellationGeneration,
				stopped = true,
				output = output,
			}
		end
		activeEditExecutionThread = nil
		if not packed[1] then
			cancelTrackedThreads(trackedThreads)
			return {
				ok = false,
				error = tostring(packed[2]),
				output = output,
			}
		end
		if backgroundLifetimeSeconds then
			activeEditThreads = trackedThreads
			task.delay(backgroundLifetimeSeconds, function()
				if editExecutionToken == executionToken then
					cancelTrackedThreads(activeEditThreads)
					activeEditThreads = {}
				end
			end)
		else
			cancelTrackedThreads(trackedThreads)
			activeEditThreads = {}
		end

		local results = table.create(packed.n - 1)
		for i = 2, packed.n do
			results[i - 1] = serializeApiValue(packed[i])
		end
		return {
			ok = true,
			results = results,
			output = output,
			background = backgroundLifetimeSeconds ~= nil,
		}
	end

	function api.cancelLuauExecution(params, sessionGeneration)
		local operationGeneration = cancellationGeneration
		assertOperationOwnership(operationGeneration, sessionGeneration)
		local executionId = tostring(params.executionId or "")
		if executionId == "" then
			return { ok = false, error = "Missing execution id" }
		end
		local entry = retainedRunners[executionId]
		if entry == nil then
			return { ok = true, found = false, executionId = executionId }
		end
		if entry.generation ~= operationGeneration then
			return { ok = false, error = "The execution belongs to an older Renium session" }
		end
		retainedRunners[executionId] = nil
		entry.instance:Destroy()
		return { ok = true, found = true, executionId = executionId }
	end

	function api.isPlayModeRunning()
		return not RunService:IsEdit()
			or RunService:IsRunning()
			or RunService.RunState ~= Enum.RunState.Stopped
			or not (StudioTestService :: any).EditModeActive
	end

	local function waitForStopped(timeoutSeconds, operationGeneration)
		local deadline = os.clock() + timeoutSeconds
		while os.clock() < deadline do
			assertOperationOwnership(operationGeneration)
			if not api.isPlayModeRunning() then
				return true
			end
			task.wait(0.05)
		end
		return not api.isPlayModeRunning()
	end

	local function releasePlayOwnership()
		if playSession.launchNonce ~= nil and game:GetAttribute("__ReniumLaunchNonce") == playSession.launchNonce then
			game:SetAttribute("__ReniumLaunchNonce", nil)
		end
		if
			playSession.ownerRuntimeId ~= nil
			and game:GetAttribute("__ReniumEditRuntimeId") == playSession.ownerRuntimeId
		then
			game:SetAttribute("__ReniumEditRuntimeId", nil)
		end
		playSession.mode = nil
		playSession.launchNonce = nil
		playSession.owned = false
		playSession.ownerGeneration = nil
		playSession.ownerRuntimeId = nil
	end

	local function matchingOwnedPlayLaunch(ownerGeneration): boolean
		return playSession.owned
			and playSession.ownerGeneration == ownerGeneration
			and playSession.launchNonce ~= nil
			and game:GetAttribute("__ReniumLaunchNonce") == playSession.launchNonce
			and playSession.ownerRuntimeId ~= nil
			and game:GetAttribute("__ReniumEditRuntimeId") == playSession.ownerRuntimeId
	end

	local function playStatus(action)
		return {
			ok = true,
			action = action,
			running = api.isPlayModeRunning() or playSession.active,
			starting = playSession.starting,
			mode = playSession.mode,
			launchNonce = playSession.launchNonce,
			lastError = playSession.lastError,
			lastResult = playSession.lastResult,
			runState = currentRunStateText(),
			studioTest = currentStudioTestState(),
		}
	end

	local function executeStudioTest(token, mode, testArgs, numPlayers)
		local ok, result = pcall(function()
			if mode == "run" then
				return StudioTestService:ExecuteRunModeAsync(testArgs)
			elseif mode == "multi" then
				return (StudioTestService :: any):ExecuteMultiplayerTestAsync(numPlayers, testArgs)
			end
			return StudioTestService:ExecutePlayModeAsync(testArgs)
		end)
		if playSession.token ~= token then
			return
		end
		if ok then
			playSession.active = true
			playSession.starting = true
			playSession.lastResult = serializeApiValue(result)
			playSession.lastError = nil
			return
		end
		playSession.active = false
		playSession.starting = false
		playSession.lastStoppedAt = os.clock()
		playSession.lastResult = nil
		playSession.lastError = tostring(result)
		releasePlayOwnership()
	end

	local function monitorStudioTest(token)
		while playSession.token == token and playSession.active and not api.isPlayModeRunning() do
			task.wait(0.05)
		end
		if playSession.token ~= token or not playSession.active then
			return
		end
		playSession.starting = false
		while playSession.token == token and api.isPlayModeRunning() do
			task.wait(0.05)
		end
		if playSession.token == token and playSession.active then
			playSession.active = false
			playSession.starting = false
			playSession.lastStoppedAt = os.clock()
			releasePlayOwnership()
		end
	end

	function api.startStopPlay(params)
		local operationGeneration = cancellationGeneration
		assertOperationOwnership(operationGeneration)
		if playSession.active and api.isPlayModeRunning() then
			playSession.starting = false
		end
		local shouldStart = params.start == true
		local shouldStop = params.stop == true
		if shouldStart and shouldStop then
			return { ok = false, error = "Conflicting play state request" }
		end

		local action = "status"
		if shouldStart then
			action = "start"
			local requestedLaunchNonce = tostring(params.launchNonce or "")
			if
				requestedLaunchNonce ~= ""
				and (api.isPlayModeRunning() or playSession.starting)
				and (not playSession.owned or playSession.launchNonce ~= requestedLaunchNonce)
			then
				return { ok = false, error = "A different Renium test session is already active" }
			end
			if not api.isPlayModeRunning() and not playSession.starting then
				local numPlayers = tonumber(params.players)
				if numPlayers and (numPlayers % 1 ~= 0 or numPlayers < 1 or numPlayers > 8) then
					return { ok = false, error = "Multiplayer tests require an integer from 1 through 8" }
				end
				playSession.token += 1
				playSession.active = true
				playSession.starting = true
				if params.mode == "run" then
					playSession.mode = "run"
				elseif numPlayers then
					playSession.mode = "multi"
				else
					playSession.mode = "play"
				end
				playSession.lastError = nil
				playSession.lastResult = nil
				playSession.lastStartedAt = os.clock()
				if requestedLaunchNonce == "" then
					requestedLaunchNonce = HttpService:GenerateGUID(false)
				end
				playSession.launchNonce = requestedLaunchNonce
				playSession.ownerRuntimeId = tostring(runtimeContext.runtimeId or "")
				playSession.owned = true
				playSession.ownerGeneration = operationGeneration
				game:SetAttribute("__ReniumLaunchNonce", requestedLaunchNonce)
				game:SetAttribute("__ReniumEditRuntimeId", playSession.ownerRuntimeId)
				local token = playSession.token
				local mode = playSession.mode
				local testArgs = {
					__renium = {
						nonce = requestedLaunchNonce,
						editRuntimeId = playSession.ownerRuntimeId,
					},
					value = params.args,
				}
				task.spawn(function()
					executeStudioTest(token, mode, testArgs, numPlayers)
				end)
				task.spawn(monitorStudioTest, token)
			end
		elseif shouldStop then
			action = "stop"
			if
				type(params.launchNonce) == "string"
				and params.launchNonce ~= ""
				and playSession.owned
				and playSession.launchNonce ~= params.launchNonce
			then
				return { ok = false, error = "The active Studio test does not match this Renium launch" }
			end
			local attempts = {}
			local function attempt(label, callback)
				local attemptOk, attemptErr = pcall(callback)
				local errorMessage = if attemptOk then nil else tostring(attemptErr)
				attempts[#attempts + 1] = {
					method = label,
					ok = attemptOk,
					error = errorMessage,
				}
				return attemptOk
			end
			local function requestStop()
				if operationGeneration ~= cancellationGeneration then
					return
				end
				if api.isPlayModeRunning() or playSession.active or playSession.starting then
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
			end
			if params.waitForStopped == false then
				task.defer(requestStop)
				local result = playStatus(action)
				result.stopRequested = true
				result.attempts = attempts
				return result
			end
			requestStop()
			local stopped =
				waitForStopped(math.clamp(tonumber(params.timeoutSeconds) or 2, 0.1, 10), operationGeneration)
			if stopped then
				playSession.active = false
				playSession.starting = false
				playSession.lastStoppedAt = os.clock()
				releasePlayOwnership()
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

		assertOperationOwnership(operationGeneration)
		local result = playStatus(action)
		if action == "status" and playSession.owned and not result.running and not result.starting then
			releasePlayOwnership()
		end
		return result
	end

	function api.requestCancellation()
		local cancelledGeneration = cancellationGeneration
		cancellationGeneration += 1
		editExecutionToken += 1
		if activeEditExecutionThread ~= nil and coroutine.status(activeEditExecutionThread) ~= "dead" then
			pcall(task.cancel, activeEditExecutionThread)
		end
		activeEditExecutionThread = nil
		cancelTrackedThreads(activeEditThreads)
		activeEditThreads = {}
		for executionId, entry in pairs(retainedRunners) do
			if entry.generation == cancelledGeneration then
				retainedRunners[executionId] = nil
				entry.instance:Destroy()
			end
		end
		playSession.token += 1
		return cancelledGeneration
	end

	function api.cleanup(ownerGeneration)
		local cleanupGeneration = ownerGeneration or api.requestCancellation()
		if captureProbeGui ~= nil then
			captureProbeGui:Destroy()
		end
		captureProbeGui = nil
		captureProbeFrame = nil
		if matchingOwnedPlayLaunch(cleanupGeneration) then
			if api.isPlayModeRunning() or playSession.active or playSession.starting then
				pcall((StudioTestService :: any).EndTest, StudioTestService, true)
			end
			playSession.active = false
			playSession.starting = false
			releasePlayOwnership()
		elseif playSession.owned and playSession.ownerGeneration == cleanupGeneration then
			playSession.active = false
			playSession.starting = false
			releasePlayOwnership()
		end
	end

	return api
end

return BridgeRuntimeApi
