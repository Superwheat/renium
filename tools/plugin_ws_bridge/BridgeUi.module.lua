local BridgeUi = {}

local RunService = game:GetService("RunService")
local TweenService = game:GetService("TweenService")

local TWEEN_FAST = TweenInfo.new(0.14, Enum.EasingStyle.Quad, Enum.EasingDirection.Out)

local LOGO_IMAGE = "rbxthumb://type=Asset&id=140594231959629&w=150&h=150"
local TOOLBAR_ICON = LOGO_IMAGE

local BRAND = Color3.fromRGB(96, 165, 250)
local OK_GREEN = Color3.fromRGB(86, 194, 126)
local WARN_AMBER = Color3.fromRGB(226, 179, 64)
local ERROR_RED = Color3.fromRGB(224, 96, 96)
local IDLE_GREY = Color3.fromRGB(128, 128, 128)
local WHITE = Color3.fromRGB(255, 255, 255)
local BLACK = Color3.fromRGB(0, 0, 0)

local FONT_REGULAR = Font.fromName("Ubuntu")
local FONT_MEDIUM = Font.fromName("Ubuntu", Enum.FontWeight.Medium)
local FONT_BOLD = Font.fromName("Ubuntu", Enum.FontWeight.Bold)
local FONT_MONO = Font.fromName("RobotoMono")

local TEXT_LG = 20
local TEXT_MD = 18
local TEXT_SM = 16
local TEXT_XS = 14
local DESCRIPTION_TEXT = 13

local CORNER = 6
local CONTROL_HEIGHT = 32
local WIDGET_PADDING = 16
local LIST_SPACING = 12
local DROPDOWN_WIDTH = 150
local DROPDOWN_ENTRY_HEIGHT = 30
local TOGGLE_WIDTH = 40
local TOGGLE_HEIGHT = 22
local TOGGLE_KNOB = 16

local function studioColor(theme, name, fallback)
	local okStyle, style = pcall(function()
		return Enum.StudioStyleGuideColor[name]
	end)
	if not okStyle or style == nil then
		return fallback
	end
	local okColor, color = pcall(function()
		return theme:GetColor(style)
	end)
	if okColor then
		return color
	end
	return fallback
end

local function palette(theme)
	return {
		Background = studioColor(theme, "MainBackground", Color3.fromRGB(31, 31, 31)),
		Card = studioColor(theme, "Titlebar", Color3.fromRGB(40, 40, 40)),
		Field = studioColor(theme, "InputFieldBackground", Color3.fromRGB(45, 45, 45)),
		Border = studioColor(theme, "Border", Color3.fromRGB(72, 72, 72)),
		Text = studioColor(theme, "MainText", Color3.fromRGB(235, 235, 235)),
		Dimmed = studioColor(theme, "DimmedText", Color3.fromRGB(160, 160, 160)),
		Button = studioColor(theme, "Button", Color3.fromRGB(58, 58, 58)),
		ButtonText = studioColor(theme, "ButtonText", Color3.fromRGB(235, 235, 235)),
	}
end

local function addCorner(instance)
	local corner = Instance.new("UICorner")
	corner.CornerRadius = UDim.new(0, CORNER)
	corner.Parent = instance
	return corner
end

local function addStroke(instance, transparency)
	local stroke = Instance.new("UIStroke")
	stroke.Thickness = 1
	stroke.Transparency = transparency or 0.4
	stroke.ApplyStrokeMode = Enum.ApplyStrokeMode.Border
	stroke.Parent = instance
	return stroke
end

local function addPadding(instance, left, right, top, bottom)
	local padding = Instance.new("UIPadding")
	padding.PaddingLeft = UDim.new(0, left)
	padding.PaddingRight = UDim.new(0, right)
	padding.PaddingTop = UDim.new(0, top)
	padding.PaddingBottom = UDim.new(0, bottom)
	padding.Parent = instance
	return padding
end

local function addVerticalList(parent, gap)
	local layout = Instance.new("UIListLayout")
	layout.FillDirection = Enum.FillDirection.Vertical
	layout.SortOrder = Enum.SortOrder.LayoutOrder
	layout.Padding = UDim.new(0, gap)
	layout.Parent = parent
	return layout
end

local function makeChevron(parent, px, refs)
	local holder = Instance.new("Frame")
	holder.Name = "Chevron"
	holder.BackgroundTransparency = 1
	holder.Size = UDim2.fromOffset(px, px)
	local barLength = math.floor(px * 0.56 + 0.5)
	local spread = math.max(1, math.floor((barLength - 2) * 0.354 + 0.5))
	local bars = {}
	for index, rotation in ipairs({ 45, -45 }) do
		local bar = Instance.new("Frame")
		bar.Name = "Bar" .. index
		bar.AnchorPoint = Vector2.new(0.5, 0.5)
		bar.BorderSizePixel = 0
		bar.Size = UDim2.fromOffset(barLength, 2)
		bar.Position = UDim2.new(0.5, index == 1 and -spread or spread, 0.5, 0)
		bar.Rotation = rotation
		local corner = Instance.new("UICorner")
		corner.CornerRadius = UDim.new(1, 0)
		corner.Parent = bar
		bar.Parent = holder
		table.insert(bars, bar)
	end
	holder.Parent = parent
	table.insert(refs.chevrons, { holder = holder, bars = bars })
	return holder
end

local function isDarkTheme(p)
	return (0.299 * p.Background.R + 0.587 * p.Background.G + 0.114 * p.Background.B) < 0.5
end

local function newRefs()
	return {
		roots = {},
		cards = {},
		cardStrokes = {},
		dividers = {},
		text = {},
		dimmed = {},
		chevrons = {},
		fields = {},
		fieldStrokes = {},
		fieldTexts = {},
		secondaryButtons = {},
		palette = palette(settings().Studio.Theme),
	}
end

local function makeText(parent, name, text, opts)
	local label = Instance.new("TextLabel")
	label.Name = name
	label.BackgroundTransparency = 1
	label.Text = text
	label.TextWrapped = opts.wrap == true
	label.TextTruncate = opts.wrap and Enum.TextTruncate.None or Enum.TextTruncate.AtEnd
	label.TextXAlignment = opts.xAlign or Enum.TextXAlignment.Left
	label.TextYAlignment = Enum.TextYAlignment.Center
	label.FontFace = opts.font or FONT_REGULAR
	label.TextSize = opts.size or TEXT_SM
	if opts.lineHeight ~= nil then
		label.LineHeight = opts.lineHeight
	end
	if opts.autoHeight then
		label.AutomaticSize = Enum.AutomaticSize.Y
		label.Size = UDim2.new(1, 0, 0, 0)
		label.TextYAlignment = Enum.TextYAlignment.Top
	end
	label.Parent = parent
	return label
end

local function makeCard(parent, name, refs)
	local card = Instance.new("Frame")
	card.Name = name
	card.AutomaticSize = Enum.AutomaticSize.Y
	card.Size = UDim2.new(1, 0, 0, 0)
	card.BorderSizePixel = 0
	card.Parent = parent
	addCorner(card)
	local stroke = addStroke(card)
	table.insert(refs.cards, card)
	table.insert(refs.cardStrokes, stroke)
	return card
end

local function styleButton(button, refs, primary)
	button.BorderSizePixel = 0
	button.AutoButtonColor = true
	button.FontFace = FONT_MEDIUM
	button.TextSize = TEXT_SM
	addCorner(button)
	if primary then
		button.BackgroundColor3 = BRAND
		button.TextColor3 = WHITE
	else
		table.insert(refs.secondaryButtons, button)
	end
end

local function setButtonEnabled(button, enabled)
	button.Active = enabled
	button.AutoButtonColor = enabled
	button.TextTransparency = enabled and 0 or 0.4
	button.BackgroundTransparency = enabled and 0 or 0.25
end

local function createWidget(plugin, id, dockInfo, title)
	local widget
	local ok, created = pcall(function()
		return plugin:CreateDockWidgetPluginGuiAsync(id, dockInfo)
	end)
	if ok and created then
		widget = created
	else
		widget = plugin:CreateDockWidgetPluginGui(id, dockInfo)
	end
	widget.Title = title
	return widget
end

local function makeSection(parent, title, order, refs, onToggle)
	local container = Instance.new("Frame")
	container.Name = "Section_" .. title
	container.AutomaticSize = Enum.AutomaticSize.Y
	container.Size = UDim2.new(1, 0, 0, 0)
	container.BackgroundTransparency = 1
	container.LayoutOrder = order
	container.Parent = parent
	addVerticalList(container, 8)

	local header = Instance.new("TextButton")
	header.Name = "Header"
	header.Size = UDim2.new(1, 0, 0, 26)
	header.BackgroundTransparency = 1
	header.AutoButtonColor = false
	header.Text = ""
	header.LayoutOrder = 1
	header.Parent = container

	local titleLabel = makeText(header, "Title", title, { font = FONT_MEDIUM, size = TEXT_MD })
	titleLabel.Size = UDim2.new(1, -24, 1, 0)

	local chevron = makeChevron(header, 16, refs)
	chevron.AnchorPoint = Vector2.new(1, 0.5)
	chevron.Position = UDim2.new(1, -4, 0.5, 0)

	local holder = Instance.new("Frame")
	holder.Name = "ContentClip"
	holder.ClipsDescendants = true
	holder.BackgroundTransparency = 1
	holder.Size = UDim2.new(1, 0, 0, 0)
	holder.LayoutOrder = 2
	holder.Parent = container

	local content = makeCard(holder, "Content", refs)
	addVerticalList(content, 0)

	local expanded = true
	local animating = false
	local animToken = 0

	content:GetPropertyChangedSignal("AbsoluteSize"):Connect(function()
		if expanded and not animating then
			holder.Size = UDim2.new(1, 0, 0, content.AbsoluteSize.Y)
		end
	end)

	header.MouseButton1Click:Connect(function()
		expanded = not expanded
		animToken = animToken + 1
		local token = animToken
		TweenService:Create(chevron, TWEEN_FAST, { Rotation = expanded and 0 or -90 }):Play()
		animating = true

		local startHeight = math.floor(holder.AbsoluteSize.Y + 0.5)
		local baseCanvas = math.floor(parent.CanvasPosition.Y + 0.5)
		local headerLimit =
			math.floor(header.AbsolutePosition.Y - parent.AbsolutePosition.Y + baseCanvas - WIDGET_PADDING + 0.5)
		local startClock = os.clock()

		local conn
		conn = RunService.Heartbeat:Connect(function()
			if animToken ~= token then
				conn:Disconnect()
				return
			end
			local t = math.min((os.clock() - startClock) / TWEEN_FAST.Time, 1)
			local eased = 1 - (1 - t) * (1 - t)
			local liveTarget = expanded and math.floor(content.AbsoluteSize.Y + 0.5) or 0
			local height = math.floor(startHeight + (liveTarget - startHeight) * eased + 0.5)
			holder.Size = UDim2.new(1, 0, 0, height)
			if expanded then
				local desired = math.max(0, math.min(baseCanvas + height - startHeight, headerLimit))
				parent.CanvasPosition = Vector2.new(0, desired)
			end
			if t >= 1 then
				conn:Disconnect()
				animating = false
				if expanded then
					holder.Size = UDim2.new(1, 0, 0, content.AbsoluteSize.Y)
					task.defer(function()
						if animToken ~= token then
							return
						end
						local desired = baseCanvas + math.floor(content.AbsoluteSize.Y + 0.5) - startHeight
						parent.CanvasPosition = Vector2.new(0, math.max(0, math.min(desired, headerLimit)))
					end)
				end
			end
		end)
		if onToggle ~= nil then
			onToggle()
		end
	end)

	task.defer(function()
		if expanded and not animating then
			holder.Size = UDim2.new(1, 0, 0, content.AbsoluteSize.Y)
		end
	end)

	table.insert(refs.text, titleLabel)
	return content
end

local function makeRow(content, name, description, order, refs, isFirst, controlWidth)
	local row = Instance.new("Frame")
	row.Name = "Row_" .. name
	row.AutomaticSize = Enum.AutomaticSize.Y
	row.Size = UDim2.new(1, 0, 0, 0)
	row.BackgroundTransparency = 1
	row.LayoutOrder = order
	row.Parent = content

	if not isFirst then
		local divider = Instance.new("Frame")
		divider.Name = "Divider"
		divider.Size = UDim2.new(1, 0, 0, 1)
		divider.BorderSizePixel = 0
		divider.Parent = row
		table.insert(refs.dividers, divider)
	end

	local inner = Instance.new("Frame")
	inner.Name = "Inner"
	inner.AutomaticSize = Enum.AutomaticSize.Y
	inner.Size = UDim2.new(1, 0, 0, 0)
	inner.Position = UDim2.new(0, 0, 0, isFirst and 0 or 1)
	inner.BackgroundTransparency = 1
	inner.Parent = row
	addPadding(inner, 14, 14, 12, 12)

	local left = Instance.new("Frame")
	left.Name = "Left"
	left.AutomaticSize = Enum.AutomaticSize.Y
	left.Size = UDim2.new(1, -(controlWidth + 16), 0, 0)
	left.BackgroundTransparency = 1
	left.Parent = inner
	addVerticalList(left, 4)

	local nameLabel = makeText(left, "Name", name, { font = FONT_REGULAR, size = TEXT_SM, autoHeight = true, wrap = true })
	nameLabel.LayoutOrder = 1
	local descLabel =
		makeText(
			left,
			"Description",
			description,
			{ size = DESCRIPTION_TEXT, autoHeight = true, wrap = true, lineHeight = 1.08 }
		)
	descLabel.LayoutOrder = 2
	table.insert(refs.text, nameLabel)
	table.insert(refs.dimmed, descLabel)

	local controlHost = Instance.new("Frame")
	controlHost.Name = "Control"
	controlHost.AnchorPoint = Vector2.new(1, 0.5)
	controlHost.Position = UDim2.new(1, 0, 0.5, 0)
	controlHost.Size = UDim2.new(0, controlWidth, 0, CONTROL_HEIGHT)
	controlHost.BackgroundTransparency = 1
	controlHost.Parent = inner
	return controlHost
end

local function makeInput(controlHost, placeholder, refs)
	local box = Instance.new("Frame")
	box.Name = "InputBox"
	box.Size = UDim2.fromScale(1, 1)
	box.BorderSizePixel = 0
	box.Parent = controlHost
	addCorner(box)
	local stroke = addStroke(box, 0.3)

	local textBox = Instance.new("TextBox")
	textBox.Name = "Field"
	textBox.Size = UDim2.fromScale(1, 1)
	textBox.BackgroundTransparency = 1
	textBox.ClearTextOnFocus = false
	textBox.PlaceholderText = placeholder
	textBox.Text = ""
	textBox.FontFace = FONT_MONO
	textBox.TextSize = 15
	textBox.TextXAlignment = Enum.TextXAlignment.Left
	textBox.TextTruncate = Enum.TextTruncate.AtEnd
	textBox.Parent = box
	addPadding(textBox, 10, 10, 0, 0)

	textBox.Focused:Connect(function()
		stroke.Color = BRAND
		stroke.Transparency = 0
	end)
	textBox.FocusLost:Connect(function()
		stroke.Color = refs.palette.Border
		stroke.Transparency = 0.3
	end)

	table.insert(refs.fields, box)
	table.insert(refs.fieldStrokes, stroke)
	table.insert(refs.fieldTexts, textBox)
	return textBox
end

local function makeToggle(controlHost, refs)
	local button = Instance.new("TextButton")
	button.Name = "Toggle"
	button.AnchorPoint = Vector2.new(1, 0.5)
	button.Position = UDim2.new(1, 0, 0.5, 0)
	button.Size = UDim2.fromOffset(TOGGLE_WIDTH, TOGGLE_HEIGHT)
	button.BorderSizePixel = 0
	button.AutoButtonColor = false
	button.Text = ""
	button.Parent = controlHost
	local trackCorner = Instance.new("UICorner")
	trackCorner.CornerRadius = UDim.new(1, 0)
	trackCorner.Parent = button
	local stroke = addStroke(button, 0.35)

	local knob = Instance.new("Frame")
	knob.Name = "Knob"
	knob.AnchorPoint = Vector2.new(0, 0.5)
	knob.Size = UDim2.fromOffset(TOGGLE_KNOB, TOGGLE_KNOB)
	knob.Position = UDim2.new(0, 3, 0.5, 0)
	knob.BorderSizePixel = 0
	knob.BackgroundColor3 = WHITE
	knob.Parent = button
	local knobCorner = Instance.new("UICorner")
	knobCorner.CornerRadius = UDim.new(1, 0)
	knobCorner.Parent = knob

	local ON_POSITION = UDim2.new(1, -(TOGGLE_KNOB + 3), 0.5, 0)
	local OFF_POSITION = UDim2.new(0, 3, 0.5, 0)

	local value = false
	local function visuals()
		if value then
			return BRAND, 1, ON_POSITION, 0
		end
		return refs.palette.Button, 0.15, OFF_POSITION, 0.25
	end

	local function paint()
		local trackColor, strokeTransparency, knobPosition, knobTransparency = visuals()
		button.BackgroundColor3 = trackColor
		stroke.Color = refs.palette.Border
		stroke.Transparency = strokeTransparency
		knob.Position = knobPosition
		knob.BackgroundTransparency = knobTransparency
	end
	paint()

	return {
		button = button,
		get = function()
			return value
		end,
		set = function(raw)
			local nextValue = raw == true or raw == "true"
			if nextValue == value then
				return
			end
			value = nextValue
			local trackColor, strokeTransparency, knobPosition, knobTransparency = visuals()
			stroke.Color = refs.palette.Border
			TweenService:Create(button, TWEEN_FAST, { BackgroundColor3 = trackColor }):Play()
			TweenService:Create(stroke, TWEEN_FAST, { Transparency = strokeTransparency }):Play()
			TweenService
				:Create(knob, TWEEN_FAST, { Position = knobPosition, BackgroundTransparency = knobTransparency })
				:Play()
		end,
		paint = paint,
	}
end

local function makeDropdownContext(root)
	local overlay = Instance.new("Frame")
	overlay.Name = "DropdownOverlay"
	overlay.BackgroundTransparency = 1
	overlay.Size = UDim2.fromScale(1, 1)
	overlay.ZIndex = 5
	overlay.Visible = false
	overlay.Parent = root

	local backdrop = Instance.new("TextButton")
	backdrop.Name = "Backdrop"
	backdrop.BackgroundColor3 = Color3.new(0, 0, 0)
	backdrop.BackgroundTransparency = 1
	backdrop.BorderSizePixel = 0
	backdrop.AutoButtonColor = false
	backdrop.Text = ""
	backdrop.Size = UDim2.fromScale(1, 1)
	backdrop.ZIndex = 5
	backdrop.Parent = overlay

	local ctx = { root = root, overlay = overlay, currentMenu = nil }
	function ctx.closeAll()
		if ctx.currentMenu ~= nil then
			ctx.currentMenu.Visible = false
			ctx.currentMenu = nil
		end
		overlay.Visible = false
		backdrop.BackgroundTransparency = 1
	end
	function ctx.openMenu(menu)
		if ctx.currentMenu ~= nil and ctx.currentMenu ~= menu then
			ctx.currentMenu.Visible = false
		end
		ctx.currentMenu = menu
		overlay.Visible = true
		backdrop.BackgroundTransparency = 1
		TweenService:Create(backdrop, TWEEN_FAST, { BackgroundTransparency = 0.8 }):Play()
		menu.Visible = true
	end
	backdrop.MouseButton1Click:Connect(ctx.closeAll)
	return ctx
end

local function makeDropdown(controlHost, options, refs, ctx, placeholderText)
	local button = Instance.new("TextButton")
	button.Name = "Dropdown"
	button.Size = UDim2.fromScale(1, 1)
	button.BorderSizePixel = 0
	button.AutoButtonColor = false
	button.Text = ""
	button.Parent = controlHost
	addCorner(button)
	local stroke = addStroke(button, 0.35)

	local label = Instance.new("TextLabel")
	label.Name = "Value"
	label.BackgroundTransparency = 1
	label.Size = UDim2.new(1, -30, 1, 0)
	label.Position = UDim2.new(0, 10, 0, 0)
	label.Text = placeholderText or ""
	label.FontFace = FONT_REGULAR
	label.TextSize = 15
	label.TextXAlignment = Enum.TextXAlignment.Left
	label.TextYAlignment = Enum.TextYAlignment.Center
	label.TextTruncate = Enum.TextTruncate.AtEnd
	label.Parent = button

	local chevron = makeChevron(button, 12, refs)
	chevron.AnchorPoint = Vector2.new(1, 0.5)
	chevron.Position = UDim2.new(1, -8, 0.5, 0)

	local menu = Instance.new("Frame")
	menu.Name = "Menu"
	menu.AutomaticSize = Enum.AutomaticSize.Y
	menu.BorderSizePixel = 0
	menu.Visible = false
	menu.ZIndex = 6
	menu.Parent = ctx.overlay
	addCorner(menu)
	local menuStroke = addStroke(menu, 0.1)
	addPadding(menu, 5, 5, 5, 5)
	addVerticalList(menu, 3)

	local buttons = {}
	local entries = {}
	local activeValue = nil
	local paint

	for index, option in ipairs(options) do
		local entryButton = Instance.new("TextButton")
		entryButton.Name = "Option_" .. option.value
		entryButton.Size = UDim2.new(1, 0, 0, DROPDOWN_ENTRY_HEIGHT)
		entryButton.BorderSizePixel = 0
		entryButton.AutoButtonColor = false
		entryButton.Text = option.label
		entryButton.FontFace = FONT_REGULAR
		entryButton.TextSize = 15
		entryButton.TextXAlignment = Enum.TextXAlignment.Left
		entryButton.LayoutOrder = index
		entryButton.ZIndex = 7
		entryButton.Parent = menu
		addCorner(entryButton)
		addPadding(entryButton, 8, 8, 0, 0)

		local entry = { button = entryButton, value = option.value, label = option.label, hovering = false }
		buttons[option.value] = entryButton
		table.insert(entries, entry)

		entryButton.MouseEnter:Connect(function()
			entry.hovering = true
			paint()
		end)
		entryButton.MouseLeave:Connect(function()
			entry.hovering = false
			paint()
		end)
		entryButton.MouseButton1Click:Connect(function()
			ctx.closeAll()
		end)
	end

	paint = function()
		local p = refs.palette
		button.BackgroundColor3 = p.Field
		stroke.Color = p.Border
		label.TextColor3 = p.Text
		local dark = isDarkTheme(p)
		local menuBg = dark and p.Background:Lerp(WHITE, 0.1) or WHITE
		local entryIdle = dark and p.Background:Lerp(WHITE, 0.03) or menuBg:Lerp(BLACK, 0.04)
		local entryHover = dark and menuBg:Lerp(WHITE, 0.1) or menuBg:Lerp(BLACK, 0.1)
		menu.BackgroundColor3 = menuBg
		menuStroke.Color = dark and menuBg:Lerp(WHITE, 0.28) or p.Border
		menuStroke.Transparency = 0
		for _, entry in ipairs(entries) do
			entry.button.BackgroundTransparency = 0
			if entry.value == activeValue then
				entry.button.BackgroundColor3 = menuBg:Lerp(BRAND, entry.hovering and 0.38 or 0.3)
				entry.button.TextColor3 = p.Text
				entry.button.FontFace = FONT_MEDIUM
			elseif entry.hovering then
				entry.button.BackgroundColor3 = entryHover
				entry.button.TextColor3 = p.Text
				entry.button.FontFace = FONT_REGULAR
			else
				entry.button.BackgroundColor3 = entryIdle
				entry.button.TextColor3 = p.Text
				entry.button.FontFace = FONT_REGULAR
			end
		end
	end
	paint()

	local function setActive(value)
		activeValue = value
		local text = placeholderText or ""
		for _, option in ipairs(options) do
			if option.value == value then
				text = option.label
			end
		end
		label.Text = text
		paint()
	end

	local function setChevronOpen(open)
		TweenService:Create(chevron, TWEEN_FAST, { Rotation = open and 180 or 0 }):Play()
	end
	menu:GetPropertyChangedSignal("Visible"):Connect(function()
		setChevronOpen(menu.Visible)
	end)

	button.MouseEnter:Connect(function()
		TweenService
			:Create(button, TWEEN_FAST, { BackgroundColor3 = refs.palette.Field:Lerp(refs.palette.Button, 0.6) })
			:Play()
	end)
	button.MouseLeave:Connect(function()
		TweenService:Create(button, TWEEN_FAST, { BackgroundColor3 = refs.palette.Field }):Play()
	end)

	button.MouseButton1Click:Connect(function()
		if menu.Visible then
			ctx.closeAll()
			return
		end
		local rootPosition = ctx.root.AbsolutePosition
		local rootSize = ctx.root.AbsoluteSize
		local buttonPosition = button.AbsolutePosition - rootPosition
		local menuHeight = #options * (DROPDOWN_ENTRY_HEIGHT + 3) + 10
		local menuWidth = math.max(button.AbsoluteSize.X, DROPDOWN_WIDTH)
		local x = math.max(6, math.min(buttonPosition.X, rootSize.X - menuWidth - 6))
		local y = buttonPosition.Y + button.AbsoluteSize.Y + 4
		if y + menuHeight > rootSize.Y - 6 then
			y = math.max(6, buttonPosition.Y - menuHeight - 4)
		end
		menu.Size = UDim2.fromOffset(menuWidth, 0)
		menu.Position = UDim2.fromOffset(x, y - 6)
		ctx.openMenu(menu)
		TweenService:Create(menu, TWEEN_FAST, { Position = UDim2.fromOffset(x, y) }):Play()
	end)

	return buttons, setActive, paint
end

local function buildSettingsWidget(plugin)
	local refs = newRefs()

	local info = DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Float, false, true, 480, 700, 390, 440)
	local widget = createWidget(plugin, "ReniumSettings", info, "Renium Settings")
	widget.Enabled = false

	local root = Instance.new("Frame")
	root.Name = "Root"
	root.Size = UDim2.fromScale(1, 1)
	root.BorderSizePixel = 0
	root.Parent = widget
	table.insert(refs.roots, root)

	local scroll = Instance.new("ScrollingFrame")
	scroll.Name = "Content"
	scroll.Size = UDim2.fromScale(1, 1)
	scroll.BackgroundTransparency = 1
	scroll.BorderSizePixel = 0
	scroll.CanvasSize = UDim2.new()
	scroll.ScrollBarThickness = 6
	scroll.ScrollBarImageTransparency = 0.4
	scroll.ScrollingDirection = Enum.ScrollingDirection.Y
	scroll.VerticalScrollBarInset = Enum.ScrollBarInset.Always
	scroll.Parent = root
	addPadding(scroll, WIDGET_PADDING, WIDGET_PADDING, WIDGET_PADDING, WIDGET_PADDING)
	local scrollLayout = addVerticalList(scroll, WIDGET_PADDING)
	scrollLayout:GetPropertyChangedSignal("AbsoluteContentSize"):Connect(function()
		scroll.CanvasSize = UDim2.fromOffset(0, scrollLayout.AbsoluteContentSize.Y + WIDGET_PADDING * 2)
	end)

	local dropdownCtx = makeDropdownContext(root)
	scroll:GetPropertyChangedSignal("CanvasPosition"):Connect(dropdownCtx.closeAll)

	local optionButtons = {}
	local optionSetters = {}
	local settingInputs = {}
	local settingToggles = {}
	local controlPainters = {}

	local function addDropdownRow(section, name, description, order, isFirst, setting, options)
		local controlHost = makeRow(section, name, description, order, refs, isFirst, DROPDOWN_WIDTH)
		local buttons, setActive, paint = makeDropdown(controlHost, options, refs, dropdownCtx)
		optionButtons[setting] = buttons
		optionSetters[setting] = setActive
		table.insert(controlPainters, paint)
		return buttons, setActive
	end

	local function addToggleRow(section, name, description, order, isFirst, setting)
		local controlHost = makeRow(section, name, description, order, refs, isFirst, TOGGLE_WIDTH)
		local toggle = makeToggle(controlHost, refs)
		settingToggles[setting] = toggle
		optionSetters[setting] = toggle.set
		table.insert(controlPainters, toggle.paint)
		return toggle
	end

	local connection = makeSection(scroll, "Connection", 1, refs, dropdownCtx.closeAll)
	local hostHost = makeRow(connection, "Server host", "Loopback address of the Renium sync server.", 1, refs, true, 160)
	local hostBox = makeInput(hostHost, "127.0.0.1", refs)
	local portsHost = makeRow(connection, "Server ports", "WebSocket channel ports.", 2, refs, false, 160)
	local portsBox = makeInput(portsHost, "8781,8782,8783", refs)
	addToggleRow(connection, "Auto connect", "Connect to the local Renium server when this place opens.", 3, false, "autoConnect")
	addToggleRow(connection, "Auto reconnect", "Retry after an unexpected bridge disconnect. Manual Disconnect always stays disconnected.", 4, false, "autoReconnect")

	local sync = makeSection(scroll, "Sync", 2, refs, dropdownCtx.closeAll)
	addDropdownRow(sync, "Initial sync priority", "Which side wins when live sync starts.", 1, true, "initialSyncPriority", {
		{ value = "studio", label = "Studio" },
		{ value = "editor", label = "Editor" },
		{ value = "none", label = "None" },
	})
	addToggleRow(sync, "Two-way sync", "Import Studio changes back into the editor while live sync is active.", 2, false, "twoWaySync")
	addToggleRow(sync, "Syncback properties", "Include non-script property and attribute edits from Studio.", 3, false, "syncbackProperties")
	addToggleRow(sync, "Only code mode", "Track scripts and containers that contain scripts, skipping unrelated property-only changes.", 4, false, "onlyCodeMode")
	addToggleRow(sync, "Live hydrate", "Create a missing Studio instance when an editor push targets it.", 5, false, "liveHydrate")
	addToggleRow(sync, "Keep unknowns", "Do not delete Studio instances that are absent from the editor tree during a full reconcile.", 6, false, "keepUnknowns")
	addToggleRow(sync, "Override packages", "Allow editor pushes to change read-only linked package mirrors.", 7, false, "overridePackages")

	local conflicts = makeSection(scroll, "Conflicts", 3, refs, dropdownCtx.closeAll)
	local conflictHost = makeRow(
		conflicts,
		"Conflict resolution",
		"When the same script is edited in Studio and in your editor at once.",
		1,
		refs,
		true,
		DROPDOWN_WIDTH
	)
	local conflictButtons, setConflictActive, paintConflictDropdown = makeDropdown(conflictHost, {
		{ value = "filesystem", label = "Filesystem" },
		{ value = "studio", label = "Studio" },
		{ value = "prompt", label = "Ask me" },
	}, refs, dropdownCtx, "Editor setting")
	table.insert(controlPainters, paintConflictDropdown)
	setConflictActive(nil)
	addDropdownRow(conflicts, "Display prompts", "When Renium should interrupt you for a manual live-sync conflict.", 2, false, "displayPrompts", {
		{ value = "always", label = "Always" },
		{ value = "initial", label = "Initial" },
		{ value = "never", label = "Never" },
	})
	local diffHost = makeRow(conflicts, "Diff lines limit", "Maximum lines rendered per side in a conflict preview. Full backups are always preserved.", 3, refs, false, 96)
	local diffLinesLimitBox = makeInput(diffHost, "3000", refs)
	settingInputs.diffLinesLimit = diffLinesLimitBox

	local advanced = makeSection(scroll, "Advanced", 4, refs, dropdownCtx.closeAll)
	local changesHost = makeRow(advanced, "Changes threshold", "Maximum Studio changes to take through the granular fast path before Renium uses a protected full import.", 1, refs, true, 96)
	local changesThresholdBox = makeInput(changesHost, "5", refs)
	settingInputs.changesThreshold = changesThresholdBox
	addDropdownRow(advanced, "Log level", "Amount of bridge diagnostic output shown in Studio.", 2, false, "logLevel", {
		{ value = "off", label = "Off" },
		{ value = "error", label = "Error" },
		{ value = "warn", label = "Warn" },
		{ value = "info", label = "Info" },
		{ value = "debug", label = "Debug" },
		{ value = "trace", label = "Trace" },
	})

	return {
		widget = widget,
		refs = refs,
		hostBox = hostBox,
		portsBox = portsBox,
		conflictOptionButtons = conflictButtons,
		settingOptionButtons = optionButtons,
		settingInputs = settingInputs,
		settingToggles = settingToggles,
		setConflictResolutionActive = setConflictActive,
		setRuntimeSettingActive = function(setting, value)
			local setter = optionSetters[setting]
			if setter ~= nil then
				if type(value) == "boolean" then
					setter(value and "true" or "false")
				else
					setter(tostring(value or ""))
				end
			end
		end,
		setRuntimeSettingText = function(setting, value)
			local input = settingInputs[setting]
			if input ~= nil then
				input.Text = tostring(value or "")
			end
		end,
		paintSegments = function()
			for _, paint in ipairs(controlPainters) do
				paint()
			end
		end,
		closeDropdowns = dropdownCtx.closeAll,
		open = function()
			dropdownCtx.closeAll()
			widget.Enabled = true
			pcall(function()
				widget:RequestRaise()
			end)
		end,
	}
end

local function buildStatusWidget(plugin, versionText)
	local refs = newRefs()

	local info = DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Right, false, false, 400, 300, 340, 240)
	local widget = createWidget(plugin, "ReniumStatus", info, "Renium")

	local root = Instance.new("Frame")
	root.Name = "Root"
	root.Size = UDim2.fromScale(1, 1)
	root.BorderSizePixel = 0
	root.Parent = widget
	table.insert(refs.roots, root)

	local content = Instance.new("ScrollingFrame")
	content.Name = "Content"
	content.Size = UDim2.fromScale(1, 1)
	content.BackgroundTransparency = 1
	content.BorderSizePixel = 0
	content.CanvasSize = UDim2.new()
	content.ScrollBarThickness = 6
	content.ScrollBarImageTransparency = 0.4
	content.ScrollingDirection = Enum.ScrollingDirection.Y
	content.VerticalScrollBarInset = Enum.ScrollBarInset.Always
	content.Parent = root
	addPadding(content, WIDGET_PADDING, WIDGET_PADDING, WIDGET_PADDING, WIDGET_PADDING)
	local contentLayout = addVerticalList(content, LIST_SPACING)
	contentLayout:GetPropertyChangedSignal("AbsoluteContentSize"):Connect(function()
		content.CanvasSize = UDim2.fromOffset(0, contentLayout.AbsoluteContentSize.Y + WIDGET_PADDING * 2)
	end)

	local header = Instance.new("Frame")
	header.Name = "Header"
	header.Size = UDim2.new(1, 0, 0, 30)
	header.BackgroundTransparency = 1
	header.LayoutOrder = 1
	header.Parent = content

	local logo = Instance.new("ImageLabel")
	logo.Name = "Logo"
	logo.Size = UDim2.new(0, 28, 0, 28)
	logo.Position = UDim2.new(0, 0, 0.5, -14)
	logo.BackgroundTransparency = 1
	logo.Image = LOGO_IMAGE
	logo.ScaleType = Enum.ScaleType.Fit
	logo.Parent = header

	local title = makeText(header, "Title", "Renium", { font = FONT_BOLD, size = TEXT_LG })
	title.Position = UDim2.new(0, 38, 0, 0)
	title.Size = UDim2.new(1, -120, 1, 0)
	table.insert(refs.text, title)

	local versionLabel = makeText(header, "Version", versionText, {
		font = FONT_MONO,
		size = TEXT_XS,
		xAlign = Enum.TextXAlignment.Right,
	})
	versionLabel.AnchorPoint = Vector2.new(1, 0)
	versionLabel.Position = UDim2.new(1, 0, 0, 0)
	versionLabel.Size = UDim2.new(0, 76, 1, 0)
	table.insert(refs.dimmed, versionLabel)

	local card = makeCard(content, "Status", refs)
	card.LayoutOrder = 2
	addPadding(card, 14, 14, 12, 12)
	addVerticalList(card, 6)

	local titleRow = Instance.new("Frame")
	titleRow.Name = "TitleRow"
	titleRow.Size = UDim2.new(1, 0, 0, 24)
	titleRow.BackgroundTransparency = 1
	titleRow.LayoutOrder = 1
	titleRow.Parent = card

	local dot = Instance.new("Frame")
	dot.Name = "Dot"
	dot.Size = UDim2.new(0, 10, 0, 10)
	dot.Position = UDim2.new(0, 0, 0.5, -5)
	dot.BackgroundColor3 = IDLE_GREY
	dot.BorderSizePixel = 0
	dot.Parent = titleRow
	local dotCorner = Instance.new("UICorner")
	dotCorner.CornerRadius = UDim.new(1, 0)
	dotCorner.Parent = dot

	local statusTitle = makeText(titleRow, "StatusTitle", "Disconnected", { font = FONT_MEDIUM, size = TEXT_MD })
	statusTitle.Position = UDim2.new(0, 20, 0, 0)
	statusTitle.Size = UDim2.new(1, -20, 1, 0)
	table.insert(refs.text, statusTitle)

	local statusSubtitle = makeText(card, "StatusSubtitle", "Start the sync server, then connect.", {
		size = TEXT_XS,
		autoHeight = true,
		wrap = true,
	})
	statusSubtitle.LayoutOrder = 2
	table.insert(refs.dimmed, statusSubtitle)

	local syncLine = makeText(card, "SyncedAt", "Not connected", { size = TEXT_XS })
	syncLine.Size = UDim2.new(1, 0, 0, 18)
	syncLine.LayoutOrder = 3
	table.insert(refs.dimmed, syncLine)

	local actions = Instance.new("Frame")
	actions.Name = "Actions"
	actions.Size = UDim2.new(1, 0, 0, CONTROL_HEIGHT)
	actions.BackgroundTransparency = 1
	actions.LayoutOrder = 3
	actions.Parent = content

	local primarySlot = Instance.new("Frame")
	primarySlot.Name = "PrimarySlot"
	primarySlot.Size = UDim2.new(1, -104, 1, 0)
	primarySlot.BackgroundTransparency = 1
	primarySlot.Parent = actions

	local connectButton = Instance.new("TextButton")
	connectButton.Name = "Connect"
	connectButton.Size = UDim2.fromScale(1, 1)
	connectButton.Text = "Connect"
	connectButton.Parent = primarySlot
	styleButton(connectButton, refs, true)

	local disconnectButton = Instance.new("TextButton")
	disconnectButton.Name = "Disconnect"
	disconnectButton.Size = UDim2.fromScale(1, 1)
	disconnectButton.Text = "Disconnect"
	disconnectButton.Visible = false
	disconnectButton.Parent = primarySlot
	styleButton(disconnectButton, refs, false)

	local settingsButton = Instance.new("TextButton")
	settingsButton.Name = "Settings"
	settingsButton.AnchorPoint = Vector2.new(1, 0)
	settingsButton.Position = UDim2.new(1, 0, 0, 0)
	settingsButton.Size = UDim2.new(0, 96, 1, 0)
	settingsButton.Text = "Settings"
	settingsButton.Parent = actions
	styleButton(settingsButton, refs, false)

	return {
		widget = widget,
		refs = refs,
		dot = dot,
		statusTitle = statusTitle,
		statusSubtitle = statusSubtitle,
		syncLine = syncLine,
		settingsButton = settingsButton,
		disconnectButton = disconnectButton,
		connectButton = connectButton,
	}
end

local function applyThemeToRefs(refs, p)
	refs.palette = p
	for _, frame in ipairs(refs.roots) do
		frame.BackgroundColor3 = p.Background
	end
	for _, card in ipairs(refs.cards) do
		card.BackgroundColor3 = p.Card
	end
	for _, stroke in ipairs(refs.cardStrokes) do
		stroke.Color = p.Border
	end
	for _, divider in ipairs(refs.dividers) do
		divider.BackgroundColor3 = p.Border
	end
	for _, label in ipairs(refs.text) do
		label.TextColor3 = p.Text
	end
	for _, label in ipairs(refs.dimmed) do
		label.TextColor3 = p.Dimmed
	end
	for _, chevron in ipairs(refs.chevrons) do
		for _, bar in ipairs(chevron.bars) do
			bar.BackgroundColor3 = p.Dimmed
		end
	end
	for _, field in ipairs(refs.fields) do
		field.BackgroundColor3 = p.Field
	end
	for _, stroke in ipairs(refs.fieldStrokes) do
		stroke.Color = p.Border
	end
	for _, box in ipairs(refs.fieldTexts) do
		box.TextColor3 = p.Text
		box.PlaceholderColor3 = p.Dimmed
	end
	for _, button in ipairs(refs.secondaryButtons) do
		button.BackgroundColor3 = p.Button
		button.TextColor3 = p.ButtonText
	end
end

function BridgeUi.create(plugin, _themeModule, bridgeInfo)
	bridgeInfo = bridgeInfo or {}
	local versionText = ""
	if bridgeInfo.version ~= nil and tostring(bridgeInfo.version) ~= "" then
		versionText = "v" .. tostring(bridgeInfo.version)
	end
	local tooltipVersion = versionText ~= "" and (" %s"):format(versionText) or ""
	if bridgeInfo.buildUnix ~= nil then
		tooltipVersion = ("%s build %s"):format(tooltipVersion, tostring(bridgeInfo.buildUnix))
	end

	local toolbar = plugin:CreateToolbar("Renium")
	local openButton = toolbar:CreateButton("Renium", "Open or close Renium" .. tooltipVersion, TOOLBAR_ICON)

	local settingsUi = buildSettingsWidget(plugin)
	local statusUi = buildStatusWidget(plugin, versionText)

	statusUi.settingsButton.MouseButton1Click:Connect(function()
		settingsUi.open()
	end)

	local ui = {
		openButton = openButton,
		settingsWidget = settingsUi.widget,
		openSettings = settingsUi.open,
		panelConnectButton = statusUi.connectButton,
		panelDisconnectButton = statusUi.disconnectButton,
		hostBox = settingsUi.hostBox,
		portsBox = settingsUi.portsBox,
		statusLabel = statusUi.statusSubtitle,
		conflictOptionButtons = settingsUi.conflictOptionButtons,
		settingOptionButtons = settingsUi.settingOptionButtons,
		settingInputs = settingsUi.settingInputs,
		settingToggles = settingsUi.settingToggles,
		_lastView = nil,
		_playModeHidden = false,
		_conflictValue = nil,
	}

	ui.setConflictResolutionActive = function(value)
		ui._conflictValue = value
		settingsUi.setConflictResolutionActive(value)
	end

	ui.setRuntimeSettingActive = settingsUi.setRuntimeSettingActive
	ui.setRuntimeSettingText = settingsUi.setRuntimeSettingText

	function ui.showWidget()
		if ui._playModeHidden then
			statusUi.widget.Enabled = false
			pcall(function()
				openButton:SetActive(false)
			end)
			return
		end
		statusUi.widget.Enabled = true
		pcall(function()
			openButton:SetActive(true)
		end)
		pcall(function()
			statusUi.widget:RequestRaise()
		end)
	end

	function ui.hideWidget()
		statusUi.widget.Enabled = false
		settingsUi.widget.Enabled = false
		pcall(function()
			openButton:SetActive(false)
		end)
	end

	function ui.toggleWidget()
		if ui._playModeHidden then
			ui.hideWidget()
			return
		end
		if statusUi.widget.Enabled then
			ui.hideWidget()
		else
			ui.showWidget()
		end
	end

	function ui.setPlayModeHidden(hidden)
		ui._playModeHidden = hidden == true
		if ui._playModeHidden then
			ui.hideWidget()
		end
	end

	statusUi.widget:GetPropertyChangedSignal("Enabled"):Connect(function()
		pcall(function()
			openButton:SetActive(statusUi.widget.Enabled and not ui._playModeHidden)
		end)
	end)
	pcall(function()
		openButton:SetActive(statusUi.widget.Enabled and not ui._playModeHidden)
	end)

	openButton.Click:Connect(function()
		ui.toggleWidget()
	end)

	function ui.updateStatus(view)
		ui._lastView = view
		local mode = tostring(view.mode or "disconnected")
		local color = IDLE_GREY
		if mode == "connected" then
			color = OK_GREEN
		elseif mode == "connecting" then
			color = WARN_AMBER
		elseif string.find(tostring(view.connectionStatus or ""), "interrupted", 1, true) ~= nil then
			color = ERROR_RED
		end

		statusUi.dot.BackgroundColor3 = color
		statusUi.statusTitle.Text = tostring(view.title or "Disconnected")
		statusUi.statusSubtitle.Text = tostring(view.subtitle or "")
		statusUi.syncLine.Text = tostring(view.syncText or "")

		if mode == "connected" then
			statusUi.connectButton.Visible = false
			statusUi.disconnectButton.Visible = true
			statusUi.disconnectButton.Text = "Disconnect"
			setButtonEnabled(statusUi.disconnectButton, true)
		elseif mode == "connecting" then
			statusUi.connectButton.Visible = false
			statusUi.disconnectButton.Visible = true
			statusUi.disconnectButton.Text = "Cancel"
			setButtonEnabled(statusUi.disconnectButton, true)
		else
			statusUi.connectButton.Visible = true
			statusUi.connectButton.Text = "Connect"
			statusUi.disconnectButton.Visible = false
			setButtonEnabled(statusUi.connectButton, true)
		end
	end

	function ui.applyStudioTheme()
		local p = palette(settings().Studio.Theme)
		applyThemeToRefs(statusUi.refs, p)
		applyThemeToRefs(settingsUi.refs, p)
		settingsUi.paintSegments()
		if ui._lastView ~= nil then
			ui.updateStatus(ui._lastView)
		end
	end

	ui.applyStudioTheme()
	settings().Studio.ThemeChanged:Connect(ui.applyStudioTheme)

	return ui
end

return BridgeUi
