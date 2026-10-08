local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")
local scheduler = require("@lune/task")

local Vector2 = roblox.Vector2
local Enum = roblox.Enum
local INSET = Vector2.new(0, 58)
local CENTER = Vector2.new(640, 360)

local SUPERCLASS = {
	CoreGui = "BasePlayerGui",
	PlayerGui = "BasePlayerGui",
	ScreenGui = "LayerCollector",
	Frame = "GuiObject",
	TextLabel = "GuiObject",
	TextBox = "GuiObject",
	ImageButton = "GuiButton",
	TextButton = "GuiButton",
	GuiButton = "GuiObject",
}

-- What the engine does, as read from Studio's VirtualInput: pointer events
-- refuse a point over a CoreGui GuiButton or Active element, keys refuse while
-- a menu is open, a CoreGui TextBox is focused or a rendered CoreGui object is
-- selected. That a hidden selection stops counting is an assumption the
-- maintainer verifies live.
local engine = {}
local coreGui
local playerGui

local Fake = {}
Fake.__index = function(instance, key)
	local value = rawget(instance, "props")[key]
	if value ~= nil then
		return value
	end
	return Fake[key]
end
Fake.__newindex = function(instance, key, value)
	local props = rawget(instance, "props")
	if key == "Parent" then
		if props.Destroyed then
			error(`The Parent property of {props.Name} is locked`)
		end
		if
			engine.refuseCoreMoves
			and instance:IsDescendantOf(coreGui)
			and value ~= nil
			and value ~= coreGui
			and not value:IsDescendantOf(coreGui)
		then
			error(`The Parent property of {props.Name} is locked`)
		end
		local previous = props.Parent
		if previous ~= nil then
			local siblings = rawget(previous, "children")
			table.remove(siblings, table.find(siblings, instance))
		end
		if value ~= nil then
			table.insert(rawget(value, "children"), instance)
		end
	end
	props[key] = value
end
function Fake:IsA(className)
	local current = self.ClassName
	while current ~= nil do
		if current == className then
			return true
		end
		current = SUPERCLASS[current]
	end
	return className == "Instance"
end
function Fake:GetChildren()
	return table.clone(rawget(self, "children"))
end
function Fake:GetDescendants()
	local result = {}
	local function walk(node)
		for _, child in rawget(node, "children") do
			result[#result + 1] = child
			walk(child)
		end
	end
	walk(self)
	return result
end
function Fake:FindFirstChildOfClass(className)
	for _, child in rawget(self, "children") do
		if child.ClassName == className then
			return child
		end
	end
	return nil
end
function Fake:FindFirstAncestorWhichIsA(className)
	local current = self.Parent
	while current ~= nil and not current:IsA(className) do
		current = current.Parent
	end
	return current
end
function Fake:FindFirstAncestorOfClass(className)
	local current = self.Parent
	while current ~= nil and current.ClassName ~= className do
		current = current.Parent
	end
	return current
end
function Fake:IsDescendantOf(ancestor)
	local current = self.Parent
	while current ~= nil do
		if current == ancestor then
			return true
		end
		current = current.Parent
	end
	return false
end
function Fake:GetFullName()
	local parts = {}
	local current = self
	while current ~= nil do
		table.insert(parts, 1, current.Name)
		current = current.Parent
	end
	return table.concat(parts, ".")
end
function Fake:GetDebugId()
	return rawget(self, "debugId")
end
function Fake:Destroy()
	self.Parent = nil
	rawget(self, "props").Destroyed = true
end
function Fake:WaitForChild()
	return nil
end
function Fake:ReleaseFocus()
	if engine.focusedTextBox == self then
		engine.focusedTextBox = nil
	end
end
local function rendered(object)
	local current = object
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
function Fake:GetGuiObjectsAtPosition(x, y)
	local layers = {}
	local function collect(node)
		for _, child in rawget(node, "children") do
			if child:IsA("ScreenGui") then
				if child.Enabled then
					layers[#layers + 1] = child
				end
			elseif not child:IsA("GuiObject") then
				collect(child)
			end
		end
	end
	collect(self)
	table.sort(layers, function(a, b)
		return a.DisplayOrder > b.DisplayOrder
	end)
	local hits = {}
	for _, layer in layers do
		local layerHits = {}
		for _, object in layer:GetDescendants() do
			if object:IsA("GuiObject") and rendered(object) then
				local position, size = object.AbsolutePosition, object.AbsoluteSize
				if x >= position.X and y >= position.Y and x <= position.X + size.X and y <= position.Y + size.Y then
					table.insert(layerHits, 1, object)
				end
			end
		end
		table.move(layerHits, 1, #layerHits, #hits + 1, hits)
	end
	return hits
end

local function newSignal()
	local handlers = {}
	return {
		Connect = function(_, handler)
			handlers[#handlers + 1] = handler
			return {
				Disconnect = function()
					local index = table.find(handlers, handler)
					if index then
						table.remove(handlers, index)
					end
				end,
			}
		end,
		Fire = function(_, ...)
			for _, handler in table.clone(handlers) do
				handler(...)
			end
		end,
	}
end

local nextDebugId = 0
local function new(className, parent, props)
	nextDebugId += 1
	local instance = setmetatable({
		props = { Name = className, ClassName = className },
		children = {},
		debugId = `debug-{nextDebugId}`,
	}, Fake)
	local raw = rawget(instance, "props")
	if instance:IsA("GuiObject") then
		raw.Visible = true
		raw.Active = false
		raw.ClipsDescendants = false
		raw.ContentText = ""
		raw.AbsolutePosition = Vector2.new(0, 0)
		raw.AbsoluteSize = Vector2.new(0, 0)
	end
	if instance:IsA("GuiButton") then
		raw.Activated = newSignal()
		raw.MouseButton1Click = newSignal()
	end
	if instance:IsA("ScreenGui") then
		raw.Enabled = true
		raw.DisplayOrder = 0
	end
	for key, value in props or {} do
		raw[key] = value
	end
	if parent ~= nil then
		instance.Parent = parent
	end
	return instance
end
local function at(x, y, width, height)
	return Vector2.new(x, y), Vector2.new(width, height)
end

local function coreGuiHit(position)
	for _, object in coreGui:GetGuiObjectsAtPosition(position.X - INSET.X, position.Y - INSET.Y) do
		if object:IsA("GuiButton") or object.Active then
			return true
		end
	end
	return false
end
local function coreKeyboardFocus()
	local selected = engine.selectedCore
	return engine.menuOpen
		or (selected ~= nil and selected:IsDescendantOf(coreGui) and rendered(selected))
		or (engine.focusedTextBox ~= nil and engine.focusedTextBox:IsDescendantOf(coreGui))
end
local virtualInput = {}
function virtualInput:SendMouseButton(position, button, down)
	if coreGuiHit(position) then
		error(("VirtualInput::SendMouseButton: position (%.1f, %.1f) hits CoreGUI."):format(position.X, position.Y))
	end
	if (engine.buttons[button.Name] or false) == down then
		error("VirtualInput::SendMouseButton: duplicate button state.")
	end
	engine.buttons[button.Name] = down
	table.insert(engine.delivered, `{button.Name} {if down then "down" else "up"}`)
	if button == Enum.UserInputType.MouseButton1 then
		local target = nil
		for _, object in playerGui:GetGuiObjectsAtPosition(position.X - INSET.X, position.Y - INSET.Y) do
			if object:IsA("GuiButton") then
				target = object
				break
			end
		end
		if down then
			engine.pressTarget = target
		elseif target ~= nil and target == engine.pressTarget then
			target.MouseButton1Click:Fire()
			target.Activated:Fire()
		end
	end
end
function virtualInput:SendMousePosition(position)
	if coreGuiHit(position) then
		error(("VirtualInput::SendMousePosition: position (%.1f, %.1f) hits CoreGUI."):format(position.X, position.Y))
	end
	table.insert(engine.delivered, "move")
end
function virtualInput:SendKey(down, keyCode, repeated)
	if coreKeyboardFocus() then
		error("VirtualInput::SendKey: CoreGUI has keyboard focus. VirtualInput cannot process this key.")
	end
	if keyCode == Enum.KeyCode.Escape then
		error("VirtualInput::SendKey: key is permanently bound to a CoreGUI core action. VirtualInput cannot process this key.")
	end
	if repeated and keyCode ~= Enum.KeyCode.Backspace then
		error("VirtualInput::SendKey: isRepeatedKey is only valid for text manipulation keys.")
	end
	table.insert(engine.delivered, `{keyCode.Name} {if down then "down" else "up"}`)
end
function virtualInput:SendTextInput(text)
	if coreKeyboardFocus() then
		error("VirtualInput::SendTextInput: CoreGUI has keyboard focus. VirtualInput cannot process this key.")
	end
	table.insert(engine.delivered, `text {text}`)
end

local players = new("Players", nil, { Name = "Players" })
local localPlayer = new("Player", players, { Name = "Player1" })
local guiService = setmetatable({
	GetGuiInset = function()
		return INSET, Vector2.new(0, 0)
	end,
}, {
	__index = function(_, key)
		if key == "MenuIsOpen" then
			return engine.menuOpen
		end
		return nil
	end,
})
local services = {
	HttpService = { GenerateGUID = function() return "test-runtime" end },
	LogService = {
		MessageOut = { Connect = function() return { Disconnect = function() end } end },
		GetLogHistory = function() return {} end,
	},
	UserInputService = {
		CreateVirtualInput = function()
			return virtualInput
		end,
		GetFocusedTextBox = function()
			return engine.focusedTextBox
		end,
	},
	RunService = {
		RunState = "running",
		IsEdit = function() return false end,
		IsRunning = function() return true end,
		IsClient = function() return true end,
		Heartbeat = { Wait = function() scheduler.wait() end },
	},
	StudioTestService = { EditModeActive = false, CanLeaveTest = function() return false end },
	StarterGui = {
		GetCoreGuiEnabled = function() return true end,
		SetCoreGuiEnabled = function() end,
	},
	Players = players,
	GuiService = guiService,
	Workspace = { CurrentCamera = { ViewportSize = Vector2.new(1280, 720) } },
}
players.LocalPlayer = localPlayer
local runtimeGame = {
	GetService = function(_, name)
		return assert(services[name], name)
	end,
	GetAttribute = function() return nil end,
	SetAttribute = function() end,
}
local codec = {}
local RuntimeApi = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeRuntimeApi.module.luau"), {
	environment = {
		game = runtimeGame,
		Enum = Enum,
		Vector2 = Vector2,
		task = scheduler,
		script = { Parent = { BridgeValueCodec = codec } },
		require = function(module)
			assert(module == codec)
			return module
		end,
	},
})()
local api = RuntimeApi.create({}, {
	runtimeId = "test-runtime",
	bridgeRole = "play-client",
	assertRequestLeaseActive = function() end,
	assertSessionOwnership = function() end,
})

local function reset(options)
	coreGui = new("CoreGui", nil, { Name = "CoreGui" })
	services.CoreGui = coreGui
	if playerGui ~= nil then
		playerGui:Destroy()
	end
	playerGui = new("PlayerGui", localPlayer, { Name = "PlayerGui" })
	local hud = new("ScreenGui", playerGui, { Name = "Hud", DisplayOrder = 5 })
	local hudPosition, hudSize = at(0, -58, 1280, 720)
	new("Frame", hud, { Name = "Backdrop", Active = true, AbsolutePosition = hudPosition, AbsoluteSize = hudSize })
	engine.menuOpen = false
	engine.selectedCore = nil
	engine.focusedTextBox = nil
	engine.refuseCoreMoves = options and options.refuseCoreMoves or false
	engine.delivered = {}
	engine.buttons = {}
	engine.pressTarget = nil
end

local function favoritePrompt(name)
	local layer = new("ScreenGui", coreGui, { Name = name or "AvatarEditorPrompts", DisplayOrder = -1 })
	local framePosition, frameSize = at(0, -58, 1280, 720)
	local frame = new("Frame", layer, { Name = "PromptFrame", AbsolutePosition = framePosition, AbsoluteSize = frameSize })
	local alertPosition, alertSize = at(440, 200, 400, 220)
	local alert = new("ImageButton", frame, { Name = "Prompt", AbsolutePosition = alertPosition, AbsoluteSize = alertSize })
	local titlePosition, titleSize = at(460, 210, 360, 30)
	new("TextLabel", alert, { Name = "Title", ContentText = "Favorite Item?", AbsolutePosition = titlePosition, AbsoluteSize = titleSize })
	local prompt = { layer = layer, answers = {} }
	for index, text in { "No", "Yes" } do
		local position, size = at(460 + (index - 1) * 190, 360, 170, 48)
		local button = new("ImageButton", alert, { Name = tostring(index), AbsolutePosition = position, AbsoluteSize = size })
		new("TextLabel", button, { Name = "ButtonText", ContentText = text, AbsolutePosition = position, AbsoluteSize = size })
		button.Activated:Connect(function()
			table.insert(prompt.answers, text)
			engine.menuOpen = false
			engine.selectedCore = nil
			layer:Destroy()
		end)
		prompt[text] = button
	end
	return prompt
end

local function send(actions)
	return api.sendVirtualInput({ actions = actions })
end
local function failure(actions)
	local ok, message = pcall(send, actions)
	assert(not ok, "the input batch should fail")
	return tostring(message)
end
local function delivered()
	return table.concat(engine.delivered, ",")
end

reset()
local prompt = favoritePrompt()
engine.menuOpen = true
engine.selectedCore = prompt.No
local result = send({
	{ type = "dismiss" },
	{ type = "key", key = "E", down = true },
	{ type = "key", key = "E", down = false },
})
assert(result.ok and result.verifiedClicks == 1, "dismiss presses the prompt's own button with a verified click")
assert(prompt.answers[1] == "No" and #prompt.answers == 1, "dismiss answers No, not Yes")
assert(prompt.layer.Parent == nil, "the prompt closed itself")
assert(result.coreGui.pressed[1].button == "CoreGui.AvatarEditorPrompts.PromptFrame.Prompt.1", "pressed path")
assert(result.coreGui.pressed[1].label == "No" and result.coreGui.hidden == nil, "nothing hidden when the press worked")
assert(result.coreGui.holdsKeyboard == false and result.coreGui.menuOpen == nil, "keys reach the game again")
assert(delivered() == "MouseButton1 down,MouseButton1 up,E down,E up", "probes deliver nothing: " .. delivered())

reset({ refuseCoreMoves = true })
prompt = favoritePrompt()
engine.selectedCore = prompt.No
local chat = new("ScreenGui", coreGui, { Name = "ExperienceChat" })
local chatBox = new("TextBox", chat, { Name = "ChatInputBar" })
engine.focusedTextBox = chatBox
result = send({ { type = "dismiss" }, { type = "key", key = "E", down = true }, { type = "key", key = "E", down = false } })
assert(#prompt.answers == 0 and prompt.layer.Parent == coreGui, "a refused move leaves the prompt in CoreGui")
assert(prompt.layer.Enabled == false and prompt.layer.DisplayOrder == -1, "the prompt's ScreenGui is disabled, its order kept")
assert(result.coreGui.hidden[1] == "CoreGui.AvatarEditorPrompts" and #result.coreGui.hidden == 1, "hidden path")
assert(string.find(result.coreGui.pressError, "locked", 1, true), "the press failure is reported: " .. tostring(result.coreGui.pressError))
assert(result.coreGui.focusReleased == "CoreGui.ExperienceChat.ChatInputBar" and engine.focusedTextBox == nil, "CoreGui focus released")
assert(result.coreGui.holdsKeyboard == false, "hiding the selected prompt frees the keyboard")
assert(delivered() == "E down,E up", "keys after the dismiss reach the game: " .. delivered())

reset({ refuseCoreMoves = true })
prompt = favoritePrompt()
engine.menuOpen = true
result = send({ { type = "dismiss" } })
assert(result.coreGui.hidden[1] == "CoreGui.AvatarEditorPrompts", "the prompt is hidden")
assert(result.coreGui.holdsKeyboard == true and result.coreGui.menuOpen == true, "an open Roblox menu still holds the keyboard")
local message = failure({ { type = "key", key = "E", down = true } })
assert(string.find(message, "CoreGUI has keyboard focus", 1, true) and string.find(message, "inp dismiss", 1, true), message)
engine.menuOpen = false
message = failure({ { type = "key", key = "Escape", down = true } })
assert(string.find(message, "Roblox keeps this key", 1, true), message)
assert(delivered() == "", "refused input delivers nothing: " .. delivered())

reset()
local robloxGui = new("ScreenGui", coreGui, { Name = "RobloxGui" })
local topBarPosition, topBarSize = at(0, -58, 1280, 58)
local topBar = new("Frame", robloxGui, { Name = "TopBar", Active = true, AbsolutePosition = topBarPosition, AbsoluteSize = topBarSize })
local toastPosition, toastSize = at(500, 250, 280, 120)
local toast = new("Frame", robloxGui, { Name = "Toast", Active = true, AbsolutePosition = toastPosition, AbsoluteSize = toastSize })
local cancel = new("TextButton", toast, { Name = "Cancel", ContentText = "Cancel", AbsolutePosition = toastPosition, AbsoluteSize = Vector2.new(280, 40) })
local cancelled = 0
cancel.Activated:Connect(function()
	cancelled += 1
end)
result = send({ { type = "dismiss" } })
assert(result.coreGui.hidden[1] == "CoreGui.RobloxGui.Toast" and #result.coreGui.hidden == 1, "only the covering branch is hidden")
assert(toast.Visible == false and topBar.Visible and robloxGui.Enabled and robloxGui.Parent == coreGui, "shared Roblox UI stays")
assert(cancelled == 0 and result.coreGui.pressed == nil, "a shared ScreenGui is never moved to press its buttons")
toast.Visible = true
message = failure({ { type = "press", label = "Cancel" } })
assert(string.find(message, "shares its ScreenGui", 1, true), message)

reset()
local folder = new("Folder", coreGui, { Name = "Prompts" })
local notice = new("ScreenGui", folder, { Name = "Notice", DisplayOrder = 3 })
local panelPosition, panelSize = at(540, 250, 200, 100)
local panel = new("ImageButton", notice, { Name = "Panel", AbsolutePosition = panelPosition, AbsoluteSize = panelSize })
local confirmPosition, confirmSize = at(560, 290, 160, 40)
local confirm = new("TextButton", panel, { Name = "Confirm", ContentText = "OK", AbsolutePosition = confirmPosition, AbsoluteSize = confirmSize })
local confirmed = 0
confirm.Activated:Connect(function()
	confirmed += 1
end)
result = send({ { type = "dismiss" } })
assert(confirmed == 1 and result.verifiedClicks == 1, "the prompt's OK button was pressed")
assert(notice.Parent == folder and notice.DisplayOrder == 3, "a prompt that stays open goes back where it was")
assert(notice.Enabled == false and result.coreGui.hidden[1] == "CoreGui.Prompts.Notice", "and is then hidden")
assert(string.find(result.coreGui.pressError, "stayed open", 1, true), tostring(result.coreGui.pressError))

reset()
result = send({ { type = "dismiss" } })
assert(result.coreGui.holdsKeyboard == false and result.coreGui.pressed == nil and result.coreGui.hidden == nil, "nothing to dismiss")
assert(delivered() == "", "an idle dismiss delivers nothing: " .. delivered())

reset()
prompt = favoritePrompt()
result = send({ { type = "press", label = "yes" } })
assert(prompt.answers[1] == "Yes" and result.coreGui.pressed[1].label == "Yes", "press by label")
reset()
prompt = favoritePrompt()
result = send({ { type = "press", x = 545, y = 442 } })
assert(prompt.answers[1] == "No" and result.coreGui.pressed[1].button == "CoreGui.AvatarEditorPrompts.PromptFrame.Prompt.1", "press by point")
reset()
favoritePrompt("First")
favoritePrompt("Second")
message = failure({ { type = "press", label = "No" } })
assert(string.find(message, "matches 2 Roblox buttons", 1, true), message)
message = failure({ { type = "press", label = "Maybe" } })
assert(string.find(message, "No visible Roblox button", 1, true), message)

print("Renium CoreGui dismiss tests passed")
