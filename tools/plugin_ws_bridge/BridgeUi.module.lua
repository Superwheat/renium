local BridgeUi = {}

local RunService = game:GetService("RunService")
local TweenService = game:GetService("TweenService")
local StudioService = game:GetService("StudioService")

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

local APPLY_GREEN = Color3.fromRGB(46, 158, 91)
local REVIEW_COUNTDOWN_SECONDS = 90
local REVIEW_DECISION_TTL_SECONDS = 900
local MAX_FINISHED_REVIEW_DECISIONS = 128
local REVIEW_ROW = 24
local REVIEW_OVERSCAN = 6

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
	local style = Enum.StudioStyleGuideColor[name]
	return if style then theme:GetColor(style) else fallback
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
	label.TextWrapped = not not opts.wrap
	label.TextTruncate = opts.wrap and Enum.TextTruncate.None or Enum.TextTruncate.AtEnd
	label.TextXAlignment = opts.xAlign or Enum.TextXAlignment.Left
	label.TextYAlignment = Enum.TextYAlignment.Center
	label.FontFace = opts.font or FONT_REGULAR
	label.TextSize = opts.size or TEXT_SM
	if opts.lineHeight then
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
	local ok, created = pcall(function()
		return plugin:CreateDockWidgetPluginGuiAsync(id, dockInfo)
	end)
	local widget = if ok and created then created else plugin:CreateDockWidgetPluginGui(id, dockInfo)
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
		if onToggle then
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
	textBox.FontFace = FONT_REGULAR
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

local function makeSelectableText(parent, name, refs)
	local textBox = Instance.new("TextBox")
	textBox.Name = name
	textBox.AutomaticSize = Enum.AutomaticSize.Y
	textBox.Size = UDim2.new(1, 0, 0, 0)
	textBox.BackgroundTransparency = 1
	textBox.ClearTextOnFocus = false
	textBox.MultiLine = true
	textBox.Text = ""
	textBox.TextEditable = false
	textBox.FontFace = FONT_REGULAR
	textBox.TextSize = TEXT_XS
	textBox.TextWrapped = true
	textBox.TextXAlignment = Enum.TextXAlignment.Left
	textBox.TextYAlignment = Enum.TextYAlignment.Top
	textBox.Parent = parent
	table.insert(refs.dimmed, textBox)
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
		if ctx.currentMenu then
			ctx.currentMenu.Visible = false
			ctx.currentMenu = nil
		end
		overlay.Visible = false
		backdrop.BackgroundTransparency = 1
	end
	function ctx.openMenu(menu)
		if ctx.currentMenu and ctx.currentMenu ~= menu then
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
		local defaultY = buttonPosition.Y + button.AbsoluteSize.Y + 4
		local y = if defaultY + menuHeight > rootSize.Y - 6
			then math.max(6, buttonPosition.Y - menuHeight - 4)
			else defaultY
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
	local portsBox = makeInput(portsHost, "8781,8782", refs)
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
	addToggleRow(advanced, "Notifications", "Show notices for waiting changes, reviews, and component problems.", 2, false, "notifications")
	addDropdownRow(advanced, "Log level", "Amount of bridge diagnostic output shown in Studio.", 3, false, "logLevel", {
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
			if setter then
				if type(value) == "boolean" then
					setter(value and "true" or "false")
				else
					setter(tostring(value or ""))
				end
			end
		end,
		setRuntimeSettingText = function(setting, value)
			local input = settingInputs[setting]
			if input then
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

	local statusSubtitle = makeSelectableText(card, "StatusSubtitle", refs)
	statusSubtitle.Text = "Start the sync server, then connect."
	statusSubtitle.LayoutOrder = 2

	local syncLine = makeText(card, "SyncedAt", "Not connected", { size = TEXT_XS })
	syncLine.Size = UDim2.new(1, 0, 0, 18)
	syncLine.LayoutOrder = 3
	table.insert(refs.dimmed, syncLine)

	local notificationCard = makeCard(content, "Notification", refs)
	notificationCard.LayoutOrder = 3
	notificationCard.Visible = false
	addPadding(notificationCard, 14, 14, 12, 12)
	addVerticalList(notificationCard, 6)

	local notificationTitle = makeText(notificationCard, "NotificationTitle", "", {
		font = FONT_MEDIUM,
		size = TEXT_SM,
		autoHeight = true,
		wrap = true,
	})
	notificationTitle.LayoutOrder = 1
	table.insert(refs.text, notificationTitle)

	local notificationBody = makeSelectableText(notificationCard, "NotificationBody", refs)
	notificationBody.LayoutOrder = 2

	local notificationActions = Instance.new("Frame")
	notificationActions.Name = "NotificationActions"
	notificationActions.Size = UDim2.new(1, 0, 0, CONTROL_HEIGHT)
	notificationActions.BackgroundTransparency = 1
	notificationActions.LayoutOrder = 3
	notificationActions.Parent = notificationCard

	local notificationActionButton = Instance.new("TextButton")
	notificationActionButton.Name = "Action"
	notificationActionButton.Size = UDim2.new(0.4, -4, 1, 0)
	notificationActionButton.Text = "Open"
	notificationActionButton.Parent = notificationActions
	styleButton(notificationActionButton, refs, true)

	local notificationSnoozeButton = Instance.new("TextButton")
	notificationSnoozeButton.Name = "Snooze"
	notificationSnoozeButton.Position = UDim2.new(0.4, 4, 0, 0)
	notificationSnoozeButton.Size = UDim2.new(0.3, -4, 1, 0)
	notificationSnoozeButton.Text = "Snooze"
	notificationSnoozeButton.Parent = notificationActions
	styleButton(notificationSnoozeButton, refs, false)

	local notificationDismissButton = Instance.new("TextButton")
	notificationDismissButton.Name = "Dismiss"
	notificationDismissButton.AnchorPoint = Vector2.new(1, 0)
	notificationDismissButton.Position = UDim2.new(1, 0, 0, 0)
	notificationDismissButton.Size = UDim2.new(0.3, -4, 1, 0)
	notificationDismissButton.Text = "Dismiss"
	notificationDismissButton.Parent = notificationActions
	styleButton(notificationDismissButton, refs, false)

	local actions = Instance.new("Frame")
	actions.Name = "Actions"
	actions.Size = UDim2.new(1, 0, 0, CONTROL_HEIGHT)
	actions.BackgroundTransparency = 1
	actions.LayoutOrder = 4
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
		notificationCard = notificationCard,
		notificationTitle = notificationTitle,
		notificationBody = notificationBody,
		notificationActionButton = notificationActionButton,
		notificationSnoozeButton = notificationSnoozeButton,
		notificationDismissButton = notificationDismissButton,
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

local function escapeRich(text)
	text = tostring(text)
	text = string.gsub(text, "&", "&amp;")
	text = string.gsub(text, "<", "&lt;")
	text = string.gsub(text, ">", "&gt;")
	return text
end

local function colorHex(color)
	return string.format(
		"#%02X%02X%02X",
		math.floor(color.R * 255 + 0.5),
		math.floor(color.G * 255 + 0.5),
		math.floor(color.B * 255 + 0.5)
	)
end

local function truncateValueText(text)
	local okLength, length = pcall(utf8.len, text)
	if okLength and length and length > 48 then
		local okOffset, offset = pcall(utf8.offset, text, 46)
		if okOffset and offset then
			return string.sub(text, 1, offset - 1) .. "\226\128\166"
		end
	end
	if #text > 48 then
		return string.sub(text, 1, 45) .. "..."
	end
	return text
end

local function formatStringValue(value)
	local okUtf8, length = pcall(utf8.len, value)
	if not okUtf8 or not length then
		return string.format("Binary data (%d bytes)", #value)
	end
	for index = 1, #value do
		local byte = string.byte(value, index)
		if byte < 32 and byte ~= 9 and byte ~= 10 and byte ~= 13 then
			return string.format("Binary data (%d bytes)", #value)
		end
	end
	return truncateValueText(value)
end

local function shortFontFamily(value)
	local family = tostring(value or "")
	local name = string.match(family, "([^/]+)%.json$") or string.match(family, "([^/]+)$") or family
	return name ~= "" and name or "Default"
end

local function formatFontValue(value)
	if typeof(value) == "Font" then
		return string.format("%s, %s, %s", shortFontFamily(value.Family), value.Weight.Name, value.Style.Name)
	end
	return string.format(
		"%s, %s, %s",
		shortFontFamily(value.family or value.Family),
		(tostring(value.weight or value.Weight or "Regular"):gsub("^Enum%.FontWeight%.", "")),
		(tostring(value.style or value.Style or "Normal"):gsub("^Enum%.FontStyle%.", ""))
	)
end

local function formatPushValue(raw)
	local kind = typeof(raw)
	if kind == "boolean" or kind == "number" then
		return tostring(raw)
	elseif kind == "string" then
		return formatStringValue(raw)
	elseif kind == "table" then
		if raw.family ~= nil or raw.Family ~= nil then
			return formatFontValue(raw)
		end
		local tag = tostring(raw._type or "")
		if tag == "Color3" then
			return string.format(
				"%d, %d, %d",
				math.floor((tonumber(raw.r) or 0) * 255 + 0.5),
				math.floor((tonumber(raw.g) or 0) * 255 + 0.5),
				math.floor((tonumber(raw.b) or 0) * 255 + 0.5)
			)
		elseif tag == "Vector3" or tag == "Vector2" then
			local parts = {}
			for _, component in ipairs({ raw.x, raw.y, raw.z }) do
				table.insert(parts, tostring(tonumber(component) or 0))
			end
			return table.concat(parts, ", ")
		elseif tag == "EnumItem" then
			local value = tostring(raw.name or raw.value or "")
			return string.match(value, "[^%.]+$") or value
		elseif tag == "Float" then
			return tostring(raw.value)
		elseif tag ~= "" then
			return tag
		end
		return "Structured value"
	end
	return tostring(raw)
end

local function formatInstancePath(instance)
	local segments = {}
	local current = instance
	while current and current ~= game do
		local name = current.Name
		local parent = current.Parent
		if parent then
			local matching = 0
			local ordinal = 0
			for _, sibling in ipairs(parent:GetChildren()) do
				if sibling.Name == name then
					matching += 1
					if sibling == current then
						ordinal = matching
					end
				end
			end
			if matching > 1 then
				name = string.format("%s[%d]", name, ordinal)
			end
		end
		table.insert(segments, 1, name)
		current = parent
	end
	return table.concat(segments, ".")
end

local function formatStructuredPath(segments, ordinals, lastIndex)
	local parts = {}
	for index = 1, math.min(lastIndex or #segments, #segments) do
		local name = tostring(segments[index])
		local ordinal = type(ordinals) == "table" and tonumber(ordinals[index]) or 1
		if ordinal ~= nil and ordinal > 1 then
			name = string.format("%s[%d]", name, ordinal)
		end
		table.insert(parts, name)
	end
	return table.concat(parts, ".")
end

local function formatLiveValue(value)
	local kind = typeof(value)
	if kind == "nil" then
		return nil
	end
	if kind == "boolean" then
		return tostring(value)
	end
	if kind == "number" then
		return string.format("%.6g", value)
	end
	if kind == "string" then
		return formatStringValue(value)
	end
	if kind == "Color3" then
		return string.format(
			"%d, %d, %d",
			math.floor(value.R * 255 + 0.5),
			math.floor(value.G * 255 + 0.5),
			math.floor(value.B * 255 + 0.5)
		)
	end
	if kind == "Vector3" then
		return string.format("%.6g, %.6g, %.6g", value.X, value.Y, value.Z)
	end
	if kind == "Vector2" then
		return string.format("%.6g, %.6g", value.X, value.Y)
	end
	if kind == "EnumItem" then
		return value.Name
	end
	if kind == "Instance" then
		return truncateValueText(formatInstancePath(value))
	end
	if kind == "BrickColor" then
		return value.Name
	end
	if kind == "UDim2" then
		return tostring(value)
	end
	if kind == "PhysicalProperties" then
		return string.format(
			"%.6g, %.6g, %.6g, %.6g, %.6g, %.6g",
			value.Density,
			value.Friction,
			value.Elasticity,
			value.FrictionWeight,
			value.ElasticityWeight,
			value.AcousticAbsorption
		)
	end
	if kind == "Font" then
		return formatFontValue(value)
	end
	if kind == "CFrame" then
		return tostring(value)
	end
	if kind == "ColorSequence" then
		local parts = table.create(#value.Keypoints)
		for index, keypoint in ipairs(value.Keypoints) do
			parts[index] = string.format(
				"%.6g:(%.6g, %.6g, %.6g)",
				keypoint.Time,
				keypoint.Value.R,
				keypoint.Value.G,
				keypoint.Value.B
			)
		end
		return table.concat(parts, "; ")
	end
	if kind == "NumberSequence" then
		local parts = table.create(#value.Keypoints)
		for index, keypoint in ipairs(value.Keypoints) do
			parts[index] = string.format("%.6g:(%.6g, %.6g)", keypoint.Time, keypoint.Value, keypoint.Envelope)
		end
		return table.concat(parts, "; ")
	end
	if kind == "Axes" then
		local parts = {}
		for _, name in ipairs({ "X", "Y", "Z" }) do
			if value[name] then
				table.insert(parts, name)
			end
		end
		return #parts > 0 and table.concat(parts, ", ") or "None"
	end
	if kind == "Faces" then
		local parts = {}
		for _, name in ipairs({ "Right", "Top", "Back", "Left", "Bottom", "Front" }) do
			if value[name] then
				table.insert(parts, name)
			end
		end
		return #parts > 0 and table.concat(parts, ", ") or "None"
	end
	if kind == "Ray" then
		return string.format(
			"Origin %.6g, %.6g, %.6g; Direction %.6g, %.6g, %.6g",
			value.Origin.X,
			value.Origin.Y,
			value.Origin.Z,
			value.Direction.X,
			value.Direction.Y,
			value.Direction.Z
		)
	end
	if kind == "UDim" or kind == "NumberRange" or kind == "Rect" then
		return tostring(value)
	end
	if kind == "table" then
		if value.family ~= nil or value.Family ~= nil then
			return formatFontValue(value)
		end
		if value.density ~= nil then
			return string.format(
				"%.6g, %.6g, %.6g",
				tonumber(value.density) or 0,
				tonumber(value.friction) or 0,
				tonumber(value.elasticity) or 0
			)
		end
		if value.customPhysics == false then
			return "Default"
		end
		local tag = tostring(value._type or "")
		if tag ~= "" then
			return tag
		end
		return "Structured value"
	end
	return kind
end

local function reviewValueParts(value)
	local kind = typeof(value)
	if kind == "Vector3" then
		return { { "X", value.X }, { "Y", value.Y }, { "Z", value.Z } }
	end
	if kind == "Vector2" then
		return { { "X", value.X }, { "Y", value.Y } }
	end
	if kind == "Color3" then
		return {
			{ "R", math.floor(value.R * 255 + 0.5) },
			{ "G", math.floor(value.G * 255 + 0.5) },
			{ "B", math.floor(value.B * 255 + 0.5) },
		}
	end
	if kind == "UDim" then
		return { { "Scale", value.Scale }, { "Offset", value.Offset } }
	end
	if kind == "UDim2" then
		return {
			{ "X Scale", value.X.Scale },
			{ "X Offset", value.X.Offset },
			{ "Y Scale", value.Y.Scale },
			{ "Y Offset", value.Y.Offset },
		}
	end
	if kind == "NumberRange" then
		return { { "Min", value.Min }, { "Max", value.Max } }
	end
	if kind == "Rect" then
		return {
			{ "Min X", value.Min.X },
			{ "Min Y", value.Min.Y },
			{ "Max X", value.Max.X },
			{ "Max Y", value.Max.Y },
		}
	end
	if kind == "PhysicalProperties" then
		return {
			{ "Density", value.Density },
			{ "Friction", value.Friction },
			{ "Elasticity", value.Elasticity },
			{ "Friction weight", value.FrictionWeight },
			{ "Elasticity weight", value.ElasticityWeight },
			{ "Acoustic absorption", value.AcousticAbsorption },
		}
	end
	if kind == "CFrame" then
		local components = { value:GetComponents() }
		local names = { "X", "Y", "Z", "R00", "R01", "R02", "R10", "R11", "R12", "R20", "R21", "R22" }
		local parts = table.create(12)
		for index, name in ipairs(names) do
			parts[index] = { name, components[index] }
		end
		return parts
	end
	if kind == "ColorSequence" then
		local parts = {}
		for index, keypoint in ipairs(value.Keypoints) do
			local prefix = string.format("Keypoint %d", index)
			table.insert(parts, { prefix .. " time", keypoint.Time })
			table.insert(parts, { prefix .. " red", keypoint.Value.R })
			table.insert(parts, { prefix .. " green", keypoint.Value.G })
			table.insert(parts, { prefix .. " blue", keypoint.Value.B })
		end
		return parts
	end
	if kind == "NumberSequence" then
		local parts = {}
		for index, keypoint in ipairs(value.Keypoints) do
			local prefix = string.format("Keypoint %d", index)
			table.insert(parts, { prefix .. " time", keypoint.Time })
			table.insert(parts, { prefix .. " value", keypoint.Value })
			table.insert(parts, { prefix .. " envelope", keypoint.Envelope })
		end
		return parts
	end
	if kind == "Axes" then
		return { { "X", value.X }, { "Y", value.Y }, { "Z", value.Z } }
	end
	if kind == "Faces" then
		return {
			{ "Right", value.Right },
			{ "Top", value.Top },
			{ "Back", value.Back },
			{ "Left", value.Left },
			{ "Bottom", value.Bottom },
			{ "Front", value.Front },
		}
	end
	if kind == "Ray" then
		return {
			{ "Origin X", value.Origin.X },
			{ "Origin Y", value.Origin.Y },
			{ "Origin Z", value.Origin.Z },
			{ "Direction X", value.Direction.X },
			{ "Direction Y", value.Direction.Y },
			{ "Direction Z", value.Direction.Z },
		}
	end
	if kind == "Font" then
		return {
			{ "Family", shortFontFamily(value.Family) },
			{ "Weight", value.Weight.Name },
			{ "Style", value.Style.Name },
		}
	end
	if kind ~= "table" then
		return nil
	end
	if value.family ~= nil or value.Family ~= nil then
		return {
			{ "Family", shortFontFamily(value.family or value.Family) },
			{ "Weight", (tostring(value.weight or value.Weight or "Regular"):gsub("^Enum%.FontWeight%.", "")) },
			{ "Style", (tostring(value.style or value.Style or "Normal"):gsub("^Enum%.FontStyle%.", "")) },
		}
	end
	local tag = tostring(value._type or "")
	if tag == "Vector3" or tag == "Vector2" then
		local parts = { { "X", value.x }, { "Y", value.y } }
		if tag == "Vector3" then
			table.insert(parts, { "Z", value.z })
		end
		return parts
	end
	if tag == "Color3" then
		return {
			{ "R", math.floor((tonumber(value.r) or 0) * 255 + 0.5) },
			{ "G", math.floor((tonumber(value.g) or 0) * 255 + 0.5) },
			{ "B", math.floor((tonumber(value.b) or 0) * 255 + 0.5) },
		}
	end
	if tag == "UDim" then
		return { { "Scale", value.scale }, { "Offset", value.offset } }
	end
	if tag == "UDim2" then
		return {
			{ "X Scale", value.xScale },
			{ "X Offset", value.xOffset },
			{ "Y Scale", value.yScale },
			{ "Y Offset", value.yOffset },
		}
	end
	if tag == "NumberRange" or value.min ~= nil and value.max ~= nil then
		return { { "Min", value.min }, { "Max", value.max } }
	end
	if value.density ~= nil then
		return {
			{ "Density", value.density },
			{ "Friction", value.friction },
			{ "Elasticity", value.elasticity },
			{ "Friction weight", value.frictionWeight },
			{ "Elasticity weight", value.elasticityWeight },
			{ "Acoustic absorption", value.acousticAbsorption },
		}
	end
	return nil
end

local function reviewValueComponents(oldValue, newValue)
	local oldParts = reviewValueParts(oldValue)
	local newParts = reviewValueParts(newValue)
	if not oldParts and not newParts then
		return nil
	end
	local oldByName = {}
	local newByName = {}
	local order = {}
	local seen = {}
	for _, part in ipairs(oldParts or {}) do
		oldByName[part[1]] = part[2]
		if not seen[part[1]] then
			seen[part[1]] = true
			table.insert(order, part[1])
		end
	end
	for _, part in ipairs(newParts or {}) do
		newByName[part[1]] = part[2]
		if not seen[part[1]] then
			seen[part[1]] = true
			table.insert(order, part[1])
		end
	end
	local components = {}
	for _, name in ipairs(order) do
		local oldPart = oldByName[name]
		local newPart = newByName[name]
		if oldPart ~= newPart then
			table.insert(components, {
				name = name,
				oldText = oldPart ~= nil and formatLiveValue(oldPart) or nil,
				newText = newPart ~= nil and formatLiveValue(newPart) or "Not set",
			})
		end
	end
	return #components > 0 and components or nil
end

local reviewIconCache = {}

local function reviewClassIconData(className)
	local cached = reviewIconCache[className]
	if cached then
		return cached
	end
	local ok, rawData = pcall(function()
		return StudioService:GetClassIcon(className)
	end)
	local data = if ok and type(rawData) == "table" then rawData else {}
	reviewIconCache[className] = data
	return data
end

local function compareReviewNodes(a, b)
	local an = string.lower(a.name)
	local bn = string.lower(b.name)
	local ap, ad = string.match(an, "^(.-)(%d+)$")
	local bp, bd = string.match(bn, "^(.-)(%d+)$")
	if ap and bp and ap == bp then
		return tonumber(ad) < tonumber(bd)
	end
	return an < bn
end

local function findOrdinalChild(parent, name, ordinal)
	if not parent then
		return nil
	end
	if ordinal <= 1 then
		return parent:FindFirstChild(name)
	end
	local seen = 0
	for _, child in ipairs(parent:GetChildren()) do
		if child.Name == name then
			seen = seen + 1
			if seen == ordinal then
				return child
			end
		end
	end
	return nil
end

local function newReviewNode(name, className, instance, ordinal)
	return {
		name = name,
		displayName = name,
		ordinal = ordinal or 1,
		className = className,
		instance = instance,
		children = {},
		childByKey = {},
		props = {},
		expanded = false,
		changeTotal = 0,
	}
end

local function buildReviewTree(summaryRows, groups, helpers)
	local roots = {}
	local rootByName = {}

	for _, row in ipairs(summaryRows) do
		local serviceName = tostring(row.service or "")
		local count = tonumber(row.count) or 0
		local note = string.format(
			"%d instances%s",
			count,
			row.allowDeletes == true and " · may remove instances" or ""
		)
		local node = newReviewNode("Reconcile " .. serviceName, serviceName, nil)
		node.summary = true
		node.note = note
		node.changeTotal = count
		table.insert(roots, node)
	end

	local authoritativeCounts = {}
	local authoritativeOrder = {}
	for _, group in ipairs(groups) do
		for _, entry in ipairs(group.entries) do
			if entry.kind == "instanceReconcile" and entry.allowDeletes == true then
				local serviceName = tostring(group.service or group.pathSegments[1] or "")
				if authoritativeCounts[serviceName] == nil then
					authoritativeCounts[serviceName] = 0
					table.insert(authoritativeOrder, serviceName)
				end
				authoritativeCounts[serviceName] += 1
			end
		end
	end
	for _, serviceName in ipairs(authoritativeOrder) do
		local count = authoritativeCounts[serviceName]
		local node = newReviewNode("Authoritative reconcile " .. serviceName, serviceName, nil)
		node.summary = true
		node.note = string.format("%d desired instances · may remove unmatched Studio instances", count)
		node.changeTotal = count
		table.insert(roots, node)
	end

	local function serviceNode(serviceName)
		local node = rootByName[serviceName]
		if not node then
			node = newReviewNode(serviceName, serviceName, game:GetService(serviceName))
			rootByName[serviceName] = node
			table.insert(roots, node)
		end
		return node
	end

	for _, group in ipairs(groups) do
		local segments = group.pathSegments
		local node = serviceNode(tostring(segments[1]))
		local resolvedInstance = nil
		if helpers and type(helpers.resolveInstance) == "function" then
			local instance = helpers.resolveInstance(group)
			if typeof(instance) == "Instance" then
				resolvedInstance = instance
			end
		end
		local desiredParentNode = node
		for i = 2, #segments do
			local name = tostring(segments[i])
			local ordinal = if type(group.pathOrdinals) == "table"
				then tonumber(group.pathOrdinals[i]) or 1
				else 1
			local key = name .. "\1" .. tostring(ordinal)
			local child = node.childByKey[key]
			if not child then
				local liveChild = i == #segments and resolvedInstance or findOrdinalChild(node.instance, name, ordinal)
				local className = if liveChild
					then liveChild.ClassName
					elseif i == #segments
					then tostring(group.className or "Folder")
					else "Folder"
				child = newReviewNode(name, className, liveChild, ordinal)
				node.childByKey[key] = child
				table.insert(node.children, child)
				local duplicateCount = 0
				for _, existing in ipairs(node.children) do
					if existing.name == name then
						duplicateCount += 1
					end
				end
				local liveDuplicateCount = 0
				if node.instance then
					for _, sibling in ipairs(node.instance:GetChildren()) do
						if sibling.Name == name then
							liveDuplicateCount += 1
						end
					end
				end
				if duplicateCount > 1 or liveDuplicateCount > 1 or ordinal > 1 then
					for _, existing in ipairs(node.children) do
						if existing.name == name then
							existing.displayName = string.format("%s[%d]", name, existing.ordinal)
						end
					end
				end
			elseif i == #segments then
				if resolvedInstance then
					child.instance = resolvedInstance
					child.className = resolvedInstance.ClassName
				elseif not child.instance then
					child.className = tostring(group.className or child.className)
				end
			end
			if i == #segments then
				desiredParentNode = node
			end
			node = child
		end
		local keepsInstance = false
		for _, entry in ipairs(group.entries) do
			if entry.kind ~= "instanceRemove" and not (entry.kind == "source" and entry.deleted == true) then
				keepsInstance = true
				break
			end
		end
		if resolvedInstance and keepsInstance then
			local desiredName = tostring(segments[#segments] or "")
			if resolvedInstance.Name ~= desiredName then
				table.insert(node.props, {
					name = "Name",
					typeName = "string",
					oldText = resolvedInstance.Name,
					newText = desiredName,
				})
			end
			local desiredParent = desiredParentNode.instance
			if desiredParent ~= nil and resolvedInstance.Parent ~= desiredParent then
				table.insert(node.props, {
					name = "Parent",
					typeName = "Instance",
					oldText = resolvedInstance.Parent ~= nil and formatInstancePath(resolvedInstance.Parent) or "Not set",
					newText = formatInstancePath(desiredParent),
				})
			elseif desiredParent == nil and #segments > 1 then
				local currentParentPath = resolvedInstance.Parent ~= nil and formatInstancePath(resolvedInstance.Parent) or "Not set"
				local desiredParentPath = formatStructuredPath(segments, group.pathOrdinals, #segments - 1)
				if currentParentPath ~= desiredParentPath then
					table.insert(node.props, {
						name = "Parent",
						typeName = "Instance",
						oldText = currentParentPath,
						newText = desiredParentPath,
					})
				end
			end
		end
		for _, entry in ipairs(group.entries) do
			if entry.kind == "instanceRemove" or (entry.kind == "source" and entry.deleted == true) then
				node.status = "removed"
			elseif entry.kind == "instanceReconcile" then
				local className = tostring(group.className or node.className)
				if not node.instance then
					node.status = "added"
				elseif node.instance.ClassName ~= className then
					table.insert(node.props, {
						name = "ClassName",
						typeName = "string",
						oldText = node.instance.ClassName,
						newText = className,
					})
				end
			elseif entry.kind == "instanceAdd" and not node.instance and not node.status then
				node.status = "added"
			elseif entry.kind == "instanceReplace" then
				local newClass = tostring(group.className or "")
				if not node.instance then
					if not node.status then
						node.status = "added"
					end
				elseif newClass == "Folder" and node.instance:IsA("LuaSourceContainer") then
					node.status = "removed"
				elseif newClass ~= "" and node.instance.ClassName ~= newClass then
					table.insert(node.props, {
						name = "ClassName",
						typeName = "string",
						oldText = node.instance.ClassName,
						newText = newClass,
					})
				end
			elseif entry.kind == "source" then
				node.sourceEdited = true
			elseif entry.kind == "property" or entry.kind == "attribute" then
				local name = tostring(entry.name or "")
				local haveOld = false
				local oldValue = nil
				local oldValueMissing = entry.oldValueMissing == true
				if entry.oldValueKnown == true then
					if oldValueMissing then
						haveOld = true
					elseif helpers and type(helpers.decodeValue) == "function" then
						local okCall, okDecode, decoded = pcall(helpers.decodeValue, entry.oldValue, name, group.service)
						if okCall and okDecode then
							haveOld = true
							oldValue = decoded
						end
					else
						haveOld = true
						oldValue = entry.oldValue
					end
				elseif node.instance then
					local instance = node.instance
					if entry.kind == "attribute" then
						haveOld, oldValue = pcall(function()
							return instance:GetAttribute(name)
						end)
					elseif helpers and type(helpers.readProperty) == "function" then
						local okCall, okRead, value = pcall(helpers.readProperty, instance, name)
						if okCall and okRead then
							haveOld = true
							oldValue = value
						end
					else
						haveOld, oldValue = pcall(function()
							return (instance :: any)[name]
						end)
					end
					if haveOld and oldValue == nil then
						oldValueMissing = true
					end
				end
				local haveNew = false
				local newValue = nil
				local deleting = entry.deleted == true
				local truncated = type(entry.value) == "table" and entry.value._reviewTruncated == true
				if deleting then
					haveNew = true
				elseif not truncated and helpers and type(helpers.decodeValue) == "function" then
					local okCall, okDecode, decoded = pcall(helpers.decodeValue, entry.value, name, group.service)
					if okCall and okDecode then
						haveNew = true
						newValue = decoded
					end
				end
				local isNoop = false
				if haveOld and haveNew then
					if helpers and type(helpers.valuesEqual) == "function" then
						isNoop = helpers.valuesEqual(oldValue, newValue)
					else
						isNoop = oldValue == newValue
					end
				end
				if
					not isNoop
					and oldValue == nil
					and type(newValue) == "table"
					and newValue.customPhysics == false
				then
					isNoop = true
				end
				if not isNoop then
					local oldText = haveOld and (oldValueMissing and "Not set" or formatLiveValue(oldValue)) or nil
					local newText = if deleting
						then "Not set"
						elseif haveNew
						then formatLiveValue(newValue) or "Default"
						elseif truncated
						then tostring(entry.value.summary or "Structured value")
						else formatPushValue(entry.value)
					local componentOldValue = haveOld and not oldValueMissing and oldValue or nil
					local componentNewValue = if deleting then nil elseif haveNew then newValue else entry.value
					local components = reviewValueComponents(componentOldValue, componentNewValue)
					local typeName = tostring(entry.dataType or entry.valueType or "")
					if typeName == "" then
						local typedValue = if haveNew then newValue elseif haveOld then oldValue else entry.value
						typeName = typeof(typedValue)
					end
					table.insert(node.props, {
						name = name,
						typeName = typeName,
						oldText = oldText,
						newText = newText,
						components = components,
						expanded = false,
					})
				end
			end
		end
		if not node.status and not node.instance and (#node.props > 0 or node.sourceEdited) then
			node.status = "added"
		end
		node.ownChanges = #node.props + ((node.status or node.sourceEdited) and 1 or 0)
	end

	local instanceCount = 0
	local function finalize(node, depth)
		local kept = {}
		for _, child in ipairs(node.children) do
			if finalize(child, depth + 1) > 0 then
				table.insert(kept, child)
			end
		end
		node.children = kept
		table.sort(node.children, compareReviewNodes)
		table.sort(node.props, function(a, b)
			return a.name < b.name
		end)
		local total = node.ownChanges or 0
		if (node.ownChanges or 0) > 0 then
			instanceCount = instanceCount + 1
		end
		for _, child in ipairs(node.children) do
			total = total + child.changeTotal
		end
		node.changeTotal = total
		return node.changeTotal
	end
	local keptRoots = {}
	local effectiveCount = 0
	for _, node in ipairs(roots) do
		if node.summary then
			table.insert(keptRoots, node)
			effectiveCount = effectiveCount + node.changeTotal
		elseif finalize(node, 1) > 0 then
			table.insert(keptRoots, node)
			effectiveCount = effectiveCount + node.changeTotal
		end
	end

	return { roots = keptRoots, effectiveCount = effectiveCount, instanceCount = instanceCount }
end

local function flattenReviewTree(roots)
	local visible = {}
	local function visit(node, depth)
		local names = { node.displayName or node.name }
		local tail = node
		while
			#tail.children == 1
			and #tail.props == 0
			and not tail.status
			and not tail.sourceEdited
			and not tail.summary
		do
			tail = tail.children[1]
			table.insert(names, tail.displayName or tail.name)
		end
		table.insert(visible, {
			kind = "node",
			node = tail,
			depth = depth,
			chain = names,
			iconClassName = node.className,
		})
		if tail.expanded then
			for _, prop in ipairs(tail.props) do
				table.insert(visible, { kind = "prop", prop = prop, node = tail, depth = depth + 1 })
				if prop.expanded and prop.components then
					for _, component in ipairs(prop.components) do
						table.insert(visible, {
							kind = "component",
							prop = component,
							node = tail,
							depth = depth + 2,
						})
					end
				end
			end
			for _, child in ipairs(tail.children) do
				visit(child, depth + 1)
			end
		end
	end
	for _, node in ipairs(roots) do
		visit(node, 0)
	end
	return visible
end

local function ensureReviewRow(reviewUi, index)
	local row = reviewUi.rowPool[index]
	if row then
		return row
	end
	local button = Instance.new("TextButton")
	button.Name = "PooledRow"
	button.BackgroundTransparency = 1
	button.BorderSizePixel = 0
	button.Text = ""
	button.AutoButtonColor = false
	button.Size = UDim2.new(1, 0, 0, REVIEW_ROW)
	button.Parent = reviewUi.scroll

	local twisty = Instance.new("Frame")
	twisty.Name = "Twisty"
	twisty.BackgroundTransparency = 1
	twisty.Size = UDim2.fromOffset(14, REVIEW_ROW)
	twisty.Parent = button
	local twistyBars = {}
	for barIndex, rotation in ipairs({ 45, -45 }) do
		local bar = Instance.new("Frame")
		bar.Name = "Bar" .. barIndex
		bar.AnchorPoint = Vector2.new(0.5, 0.5)
		bar.BorderSizePixel = 0
		bar.Size = UDim2.fromOffset(6, 2)
		bar.Position = UDim2.new(0.5, barIndex == 1 and -2 or 2, 0.5, 0)
		bar.Rotation = rotation
		local barCorner = Instance.new("UICorner")
		barCorner.CornerRadius = UDim.new(1, 0)
		barCorner.Parent = bar
		bar.Parent = twisty
		table.insert(twistyBars, bar)
	end

	local icon = Instance.new("ImageLabel")
	icon.Name = "Icon"
	icon.BackgroundTransparency = 1
	icon.Size = UDim2.fromOffset(16, 16)
	icon.Parent = button

	local label = Instance.new("TextLabel")
	label.Name = "Label"
	label.BackgroundTransparency = 1
	label.RichText = true
	label.FontFace = FONT_REGULAR
	label.TextSize = TEXT_XS
	label.TextXAlignment = Enum.TextXAlignment.Left
	label.TextYAlignment = Enum.TextYAlignment.Center
	label.TextTruncate = Enum.TextTruncate.AtEnd
	label.Parent = button

	row = { button = button, twisty = twisty, twistyBars = twistyBars, icon = icon, label = label }
	button.MouseButton1Click:Connect(function()
		local item = row.item
		if item and item.kind == "node" and (#item.node.children > 0 or #item.node.props > 0) then
			item.node.expanded = not item.node.expanded
			reviewUi.refreshList()
		elseif item and item.kind == "prop" and item.prop.components then
			item.prop.expanded = not item.prop.expanded
			reviewUi.refreshList()
		end
	end)
	reviewUi.rowPool[index] = row
	return row
end

local function bindReviewRow(reviewUi, row, item)
	local p = reviewUi.refs.palette
	local textHex = colorHex(p.Text)
	local dimHex = colorHex(p.Dimmed)
	local greenHex = colorHex(OK_GREEN)
	local redHex = colorHex(ERROR_RED)
	local indent = item.depth * 14 + 2
	row.item = item

	if item.kind == "node" then
		local node = item.node
		local hasKids = #node.children > 0 or #node.props > 0
		row.twisty.Visible = hasKids
		row.twisty.Position = UDim2.fromOffset(indent, 0)
		row.twisty.Rotation = node.expanded and 0 or -90
		for _, bar in ipairs(row.twistyBars) do
			bar.BackgroundColor3 = p.Dimmed
		end
		row.icon.Visible = true
		row.icon.Position = UDim2.new(0, indent + 16, 0.5, -8)
		local data = reviewClassIconData(item.iconClassName or node.className)
		row.icon.Image = tostring(data.Image or "")
		row.icon.ImageRectOffset = data.ImageRectOffset or Vector2.zero
		row.icon.ImageRectSize = data.ImageRectSize or Vector2.zero
		row.icon.ImageTransparency = node.status == "removed" and 0.45 or 0
		local chain = item.chain
		local html = ""
		if #chain > 1 then
			local prefix = {}
			for i = 1, #chain - 1 do
				table.insert(prefix, escapeRich(chain[i]))
			end
			html = string.format('<font color="%s">%s › </font>', dimHex, table.concat(prefix, " › "))
		end
		local leafName = escapeRich(chain[#chain])
		local nameHex = textHex
		if node.status == "added" then
			nameHex = greenHex
		elseif node.status == "removed" then
			nameHex = redHex
			leafName = `<s>{leafName}</s>`
		end
		html = html .. string.format('<font color="%s">%s</font>', nameHex, leafName)
		if node.summary then
			html = html .. string.format('  <font color="%s">%s</font>', dimHex, escapeRich(node.note or ""))
		elseif not node.expanded and hasKids and node.changeTotal > 0 then
			html = html .. string.format('  <font color="%s">%d</font>', dimHex, node.changeTotal)
		end
		row.label.Text = html
		local labelX = indent + 36
		row.label.Position = UDim2.fromOffset(labelX, 0)
		row.label.Size = UDim2.new(1, -labelX - 4, 1, 0)
	elseif item.kind == "prop" then
		local prop = item.prop
		local hasComponents = prop.components and #prop.components > 0 or false
		row.twisty.Visible = hasComponents
		row.twisty.Position = UDim2.fromOffset(indent, 0)
		row.twisty.Rotation = prop.expanded and 0 or -90
		for _, bar in ipairs(row.twistyBars) do
			bar.BackgroundColor3 = p.Dimmed
		end
		row.icon.Visible = false
		local propertyName = if prop.typeName and prop.typeName ~= ""
			then `{escapeRich(prop.name)} <font color="{dimHex}">[{escapeRich(prop.typeName)}]</font>`
			else escapeRich(prop.name)
		local html = if prop.oldText and not item.node.status
			then string.format(
				'<font color="%s">%s</font>  <s><font color="%s">%s</font></s> <font color="%s">→</font> <font color="%s">%s</font>',
				dimHex,
				propertyName,
				redHex,
				escapeRich(prop.oldText),
				dimHex,
				greenHex,
				escapeRich(prop.newText)
			)
			else string.format(
				'<font color="%s">%s</font>  <font color="%s">%s</font>',
				dimHex,
				propertyName,
				textHex,
				escapeRich(prop.newText)
			)
		row.label.Text = html
		local labelX = indent + 22
		row.label.Position = UDim2.fromOffset(labelX, 0)
		row.label.Size = UDim2.new(1, -labelX - 4, 1, 0)
	elseif item.kind == "component" then
		row.twisty.Visible = false
		row.icon.Visible = false
		local prop = item.prop
		local html = if prop.oldText and not item.node.status
			then string.format(
				'<font color="%s">%s</font>  <s><font color="%s">%s</font></s> <font color="%s">→</font> <font color="%s">%s</font>',
				dimHex,
				escapeRich(prop.name),
				redHex,
				escapeRich(prop.oldText),
				dimHex,
				greenHex,
				escapeRich(prop.newText)
			)
			else string.format(
				'<font color="%s">%s</font>  <font color="%s">%s</font>',
				dimHex,
				escapeRich(prop.name),
				textHex,
				escapeRich(prop.newText)
			)
		row.label.Text = html
		local labelX = indent + 22
		row.label.Position = UDim2.fromOffset(labelX, 0)
		row.label.Size = UDim2.new(1, -labelX - 4, 1, 0)
	end
end

local function buildReviewWidget(plugin)
	local refs = newRefs()
	local info = DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Float, false, true, 660, 620, 560, 460)
	local widget = createWidget(plugin, "ReniumReview", info, "Renium Review")
	widget.Enabled = false

	local root = Instance.new("Frame")
	root.Name = "Root"
	root.Size = UDim2.fromScale(1, 1)
	root.BorderSizePixel = 0
	root.Parent = widget
	table.insert(refs.roots, root)
	addPadding(root, WIDGET_PADDING, WIDGET_PADDING, WIDGET_PADDING, WIDGET_PADDING)

	local title = makeText(root, "Title", "Editor changes awaiting review", { font = FONT_BOLD, size = TEXT_MD })
	title.Size = UDim2.new(1, 0, 0, 22)
	table.insert(refs.text, title)

	local subtitle = makeText(root, "Subtitle", "", {
		size = DESCRIPTION_TEXT,
		autoHeight = true,
		wrap = true,
		lineHeight = 1.15,
	})
	subtitle.RichText = true
	subtitle.Position = UDim2.fromOffset(0, 28)
	table.insert(refs.dimmed, subtitle)

	local searchHost = Instance.new("Frame")
	searchHost.Name = "Search"
	searchHost.Size = UDim2.new(1, 0, 0, CONTROL_HEIGHT)
	searchHost.BackgroundTransparency = 1
	searchHost.Parent = root
	local searchBox = makeInput(searchHost, "Search changes", refs)

	local listCard = Instance.new("Frame")
	listCard.Name = "List"
	listCard.BorderSizePixel = 0
	listCard.Parent = root
	addCorner(listCard)
	local listStroke = addStroke(listCard)
	table.insert(refs.cards, listCard)
	table.insert(refs.cardStrokes, listStroke)

	local function layoutListCard()
		local searchTop = 28 + math.max(subtitle.AbsoluteSize.Y, DESCRIPTION_TEXT + 3) + 10
		searchHost.Position = UDim2.fromOffset(0, searchTop)
		local top = searchTop + CONTROL_HEIGHT + 10
		listCard.Position = UDim2.fromOffset(0, top)
		listCard.Size = UDim2.new(1, 0, 1, -(top + 58))
	end
	subtitle:GetPropertyChangedSignal("AbsoluteSize"):Connect(layoutListCard)
	layoutListCard()

	local scroll = Instance.new("ScrollingFrame")
	scroll.Name = "Rows"
	scroll.Size = UDim2.fromScale(1, 1)
	scroll.BackgroundTransparency = 1
	scroll.BorderSizePixel = 0
	scroll.CanvasSize = UDim2.new()
	scroll.ScrollBarThickness = 6
	scroll.ScrollBarImageTransparency = 0.4
	scroll.ScrollingDirection = Enum.ScrollingDirection.Y
	scroll.VerticalScrollBarInset = Enum.ScrollBarInset.Always
	scroll.Parent = listCard
	addPadding(scroll, 6, 6, 6, 6)

	local footer = Instance.new("Frame")
	footer.Name = "Footer"
	footer.AnchorPoint = Vector2.new(0, 1)
	footer.Position = UDim2.fromScale(0, 1)
	footer.Size = UDim2.new(1, 0, 0, 46)
	footer.BackgroundTransparency = 1
	footer.Parent = root

	local countdownTrack = Instance.new("Frame")
	countdownTrack.Name = "CountdownTrack"
	countdownTrack.Size = UDim2.new(1, 0, 0, 3)
	countdownTrack.BackgroundTransparency = 0.85
	countdownTrack.BorderSizePixel = 0
	countdownTrack.Parent = footer
	local trackCorner = Instance.new("UICorner")
	trackCorner.CornerRadius = UDim.new(1, 0)
	trackCorner.Parent = countdownTrack
	table.insert(refs.dividers, countdownTrack)

	local countdownBar = Instance.new("Frame")
	countdownBar.Name = "CountdownBar"
	countdownBar.Size = UDim2.new(1, 0, 0, 3)
	countdownBar.BackgroundColor3 = APPLY_GREEN
	countdownBar.BorderSizePixel = 0
	countdownBar.Parent = countdownTrack
	local barCorner = Instance.new("UICorner")
	barCorner.CornerRadius = UDim.new(1, 0)
	barCorner.Parent = countdownBar
	local countdownMask = Instance.new("UIGradient")
	countdownMask.Transparency = NumberSequence.new({
		NumberSequenceKeypoint.new(0, 0),
		NumberSequenceKeypoint.new(0.995, 0),
		NumberSequenceKeypoint.new(1, 1),
	})
	countdownMask.Parent = countdownBar

	local countdownLabel = makeText(footer, "CountdownLabel", "", { size = DESCRIPTION_TEXT })
	countdownLabel.Position = UDim2.new(0, 0, 0, 12)
	countdownLabel.Size = UDim2.new(1, -236, 1, -12)
	table.insert(refs.dimmed, countdownLabel)

	local applyButton = Instance.new("TextButton")
	applyButton.Name = "Apply"
	applyButton.AnchorPoint = Vector2.new(1, 1)
	applyButton.Position = UDim2.new(1, 0, 1, 0)
	applyButton.Size = UDim2.new(0, 124, 0, CONTROL_HEIGHT)
	applyButton.Text = "Apply"
	applyButton.BorderSizePixel = 0
	applyButton.AutoButtonColor = true
	applyButton.FontFace = FONT_MEDIUM
	applyButton.TextSize = TEXT_SM
	applyButton.BackgroundColor3 = APPLY_GREEN
	applyButton.TextColor3 = WHITE
	applyButton.Parent = footer
	addCorner(applyButton)

	local skipButton = Instance.new("TextButton")
	skipButton.Name = "Skip"
	skipButton.AnchorPoint = Vector2.new(1, 1)
	skipButton.Position = UDim2.new(1, -132, 1, 0)
	skipButton.Size = UDim2.new(0, 96, 0, CONTROL_HEIGHT)
	skipButton.Text = "Skip batch"
	skipButton.Parent = footer
	styleButton(skipButton, refs, false)

	local reviewUi = {
		widget = widget,
		refs = refs,
		root = root,
		title = title,
		subtitle = subtitle,
		scroll = scroll,
		countdownTrack = countdownTrack,
		countdownBar = countdownBar,
		countdownMask = countdownMask,
		countdownLabel = countdownLabel,
		skipButton = skipButton,
		applyButton = applyButton,
		searchBox = searchBox,
		rowPool = {},
		visibleItems = {},
		treeRoots = {},
	}

	local function renderWindow()
		local items = reviewUi.visibleItems
		local total = #items
		scroll.CanvasSize = UDim2.fromOffset(0, total * REVIEW_ROW + 12)
		local top = scroll.CanvasPosition.Y
		local height = scroll.AbsoluteSize.Y
		local first = math.max(math.floor(top / REVIEW_ROW) - REVIEW_OVERSCAN, 1)
		local last = math.min(math.ceil((top + height) / REVIEW_ROW) + REVIEW_OVERSCAN, total)
		local poolIndex = 0
		for i = first, last do
			poolIndex = poolIndex + 1
			local row = ensureReviewRow(reviewUi, poolIndex)
			row.button.Visible = true
			row.button.Position = UDim2.fromOffset(0, (i - 1) * REVIEW_ROW)
			bindReviewRow(reviewUi, row, items[i])
		end
		for i = poolIndex + 1, #reviewUi.rowPool do
			reviewUi.rowPool[i].button.Visible = false
		end
	end

	function reviewUi.refreshList()
		local items = flattenReviewTree(reviewUi.treeRoots)
		local query = string.lower(searchBox.Text)
		if query == "" then
			reviewUi.visibleItems = items
		else
			local visible = {}
			for _, item in ipairs(items) do
				local text = ""
				if item.kind == "node" then
					text = table.concat(item.chain or {}, " ")
						.. " "
						.. tostring(item.node.className or "")
						.. " "
						.. tostring(item.node.note or "")
				else
					local prop = item.prop or {}
					text = tostring(prop.name or "")
						.. " "
						.. tostring(prop.typeName or "")
						.. " "
						.. tostring(prop.oldText or "")
						.. " "
						.. tostring(prop.newText or "")
				end
				if string.find(string.lower(text), query, 1, true) ~= nil then
					visible[#visible + 1] = item
				end
			end
			reviewUi.visibleItems = visible
		end
		renderWindow()
	end

	local pendingRender = false
	local function scheduleRender()
		if pendingRender then
			return
		end
		pendingRender = true
		task.defer(function()
			pendingRender = false
			renderWindow()
		end)
	end
	scroll:GetPropertyChangedSignal("CanvasPosition"):Connect(scheduleRender)
	scroll:GetPropertyChangedSignal("AbsoluteSize"):Connect(scheduleRender)
	searchBox:GetPropertyChangedSignal("Text"):Connect(reviewUi.refreshList)

	return reviewUi
end

function BridgeUi.create(plugin, _themeModule, bridgeInfo)
	bridgeInfo = bridgeInfo or {}
	local bridgeVersion = if bridgeInfo.version ~= nil then tostring(bridgeInfo.version) else ""
	local versionText = if bridgeVersion ~= "" then "v" .. bridgeVersion else ""
	local versionTooltip = if versionText ~= "" then (" %s"):format(versionText) else ""
	local tooltipVersion = if bridgeInfo.buildUnix ~= nil
		then ("%s build %s"):format(versionTooltip, tostring(bridgeInfo.buildUnix))
		else versionTooltip

	local toolbar = plugin:CreateToolbar("Renium")
	local openButton = toolbar:CreateButton("Renium", "Open or close Renium" .. tooltipVersion, TOOLBAR_ICON)
	local actions = {
		connect = plugin:CreatePluginAction("ReniumConnect", "Connect Renium", "Connect Renium to the local sync server", TOOLBAR_ICON, true),
		disconnect = plugin:CreatePluginAction("ReniumDisconnect", "Disconnect Renium", "Disconnect Renium from the local sync server", TOOLBAR_ICON, true),
		status = plugin:CreatePluginAction("ReniumStatus", "Show Renium Status", "Open the Renium status panel", TOOLBAR_ICON, true),
		review = plugin:CreatePluginAction("ReniumReview", "Open Renium Review", "Open the pending Renium change review", TOOLBAR_ICON, true),
		reveal = plugin:CreatePluginAction("ReniumRevealScript", "Reveal Script in Editor", "Reveal the selected Studio script in the connected editor", TOOLBAR_ICON, true),
		lock = plugin:CreatePluginAction("ReniumSessionLock", "Inspect Renium Session", "Inspect or take over the active Renium session", TOOLBAR_ICON, true),
	}

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
		actions = actions,
		_lastView = nil,
		_playModeHidden = false,
		_conflictValue = nil,
	}

	local notificationKey = nil
	local notificationAction = nil
	local storedSnoozes = plugin:GetSetting("Renium.NotificationSnoozeUntil")
	local notificationSnoozeUntil = if type(storedSnoozes) == "table" then storedSnoozes else {}
	local notificationPayloads = {}
	local scheduledSnoozes = {}

	local function hideNotification()
		notificationKey = nil
		notificationAction = nil
		statusUi.notificationCard.Visible = false
	end

	local function clearNotification(key)
		local normalizedKey = tostring(key or "")
		notificationPayloads[normalizedKey] = nil
		notificationSnoozeUntil[normalizedKey] = nil
		scheduledSnoozes[normalizedKey] = nil
		plugin:SetSetting("Renium.NotificationSnoozeUntil", notificationSnoozeUntil)
		if notificationKey == normalizedKey then
			hideNotification()
		end
	end

	statusUi.notificationActionButton.MouseButton1Click:Connect(function()
		local action = notificationAction
		hideNotification()
		if action then
			action()
		end
	end)
	statusUi.notificationSnoozeButton.MouseButton1Click:Connect(function()
		if notificationKey then
			local key = notificationKey
			local payload = notificationPayloads[key]
			notificationSnoozeUntil[key] = os.time() + 300
			plugin:SetSetting("Renium.NotificationSnoozeUntil", notificationSnoozeUntil)
			hideNotification()
			if payload ~= nil then
				task.defer(function()
					ui.notify(
						key,
						payload.title,
						payload.body,
						payload.actionLabel,
						payload.action,
						payload.allowSnooze
					)
				end)
			end
			return
		end
		hideNotification()
	end)
	statusUi.notificationDismissButton.MouseButton1Click:Connect(function()
		if notificationKey ~= nil then
			clearNotification(notificationKey)
		else
			hideNotification()
		end
	end)

	function ui.notify(key, title, body, actionLabel, action, allowSnooze)
		local normalizedKey = tostring(key or "")
		local snoozeUntil = tonumber(notificationSnoozeUntil[normalizedKey]) or 0
		if snoozeUntil > os.time() then
			notificationPayloads[normalizedKey] = {
				title = title,
				body = body,
				actionLabel = actionLabel,
				action = action,
				allowSnooze = allowSnooze,
			}
			if scheduledSnoozes[normalizedKey] ~= snoozeUntil then
				scheduledSnoozes[normalizedKey] = snoozeUntil
				task.delay(math.max(0, snoozeUntil - os.time()), function()
					if scheduledSnoozes[normalizedKey] ~= snoozeUntil then
						return
					end
					scheduledSnoozes[normalizedKey] = nil
					if (tonumber(notificationSnoozeUntil[normalizedKey]) or 0) <= os.time() then
						notificationSnoozeUntil[normalizedKey] = nil
						plugin:SetSetting("Renium.NotificationSnoozeUntil", notificationSnoozeUntil)
						local payload = notificationPayloads[normalizedKey]
						notificationPayloads[normalizedKey] = nil
						if payload ~= nil then
							ui.notify(
								normalizedKey,
								payload.title,
								payload.body,
								payload.actionLabel,
								payload.action,
								payload.allowSnooze
							)
						end
					end
				end)
			end
			return false
		end
		notificationSnoozeUntil[normalizedKey] = nil
		notificationPayloads[normalizedKey] = {
			title = title,
			body = body,
			actionLabel = actionLabel,
			action = action,
			allowSnooze = allowSnooze,
		}
		notificationKey = normalizedKey
		notificationAction = action
		statusUi.notificationTitle.Text = tostring(title or "")
		statusUi.notificationBody.Text = tostring(body or "")
		statusUi.notificationActionButton.Text = tostring(actionLabel or "Open")
		statusUi.notificationActionButton.Visible = action ~= nil
		statusUi.notificationSnoozeButton.Visible = allowSnooze == true
		statusUi.notificationCard.Visible = true
		return true
	end

	function ui.dismissNotification(key)
		if key == nil then
			hideNotification()
		else
			clearNotification(key)
		end
	end

	ui.setConflictResolutionActive = function(value)
		ui._conflictValue = value
		settingsUi.setConflictResolutionActive(value)
	end

	ui.setRuntimeSettingActive = settingsUi.setRuntimeSettingActive
	ui.setRuntimeSettingText = settingsUi.setRuntimeSettingText

	function ui.showWidget()
		if ui._playModeHidden then
			statusUi.widget.Enabled = false
			openButton:SetActive(false)
			return
		end
		statusUi.widget.Enabled = true
		openButton:SetActive(true)
		pcall(function()
			statusUi.widget:RequestRaise()
		end)
	end

	function ui.hideWidget()
		statusUi.widget.Enabled = false
		settingsUi.widget.Enabled = false
		openButton:SetActive(false)
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

	local reviewUi = nil
	local reviewState = nil
	local finishedReviewDecisions = {}
	local finishedReviewOrder = {}
	local reviewCounter = 0
	local reviewCountdown = { connection = nil, tween = nil, colorTween = nil, remaining = 0 }

	local function pruneFinishedReviewDecisions()
		local now = os.clock()
		local kept = {}
		for _, reviewId in ipairs(finishedReviewOrder) do
			local record = finishedReviewDecisions[reviewId]
			if record and record.expiresAt > now then
				table.insert(kept, reviewId)
			else
				finishedReviewDecisions[reviewId] = nil
			end
		end
		while #kept > MAX_FINISHED_REVIEW_DECISIONS do
			finishedReviewDecisions[table.remove(kept, 1)] = nil
		end
		finishedReviewOrder = kept
	end

	local function rememberFinishedReview(reviewId, decision)
		pruneFinishedReviewDecisions()
		if finishedReviewDecisions[reviewId] == nil then
			table.insert(finishedReviewOrder, reviewId)
		end
		finishedReviewDecisions[reviewId] = {
			decision = decision,
			expiresAt = os.clock() + REVIEW_DECISION_TTL_SECONDS,
		}
		pruneFinishedReviewDecisions()
	end

	local function stopReviewCountdown()
		if reviewCountdown.connection then
			reviewCountdown.connection:Disconnect()
			reviewCountdown.connection = nil
		end
		if reviewCountdown.tween then
			reviewCountdown.tween:Cancel()
			reviewCountdown.tween = nil
		end
		if reviewCountdown.colorTween then
			reviewCountdown.colorTween:Cancel()
			reviewCountdown.colorTween = nil
		end
	end

	local function decideReview(decision)
		if not reviewState or reviewState.decision then
			return
		end
		reviewState.decision = decision
		stopReviewCountdown()
		if reviewUi and reviewUi.widget.Enabled then
			reviewUi.widget.Enabled = false
		end
		ui.dismissNotification("pending-review")
	end

	local function ensureReviewUi()
		if reviewUi then
			return reviewUi
		end
		reviewUi = buildReviewWidget(plugin)
		applyThemeToRefs(reviewUi.refs, palette(settings().Studio.Theme))
		reviewUi.applyButton.MouseButton1Click:Connect(function()
			decideReview("apply")
		end)
		reviewUi.skipButton.MouseButton1Click:Connect(function()
			decideReview("skip")
		end)
		return reviewUi
	end

	local function startReviewCountdown(decision, label)
		stopReviewCountdown()
		reviewCountdown.remaining = REVIEW_COUNTDOWN_SECONDS
		reviewUi.countdownBar.Size = UDim2.new(1, 0, 0, 3)
		reviewUi.countdownBar.BackgroundColor3 = OK_GREEN
		reviewUi.countdownMask.Offset = Vector2.zero
		reviewCountdown.tween = TweenService:Create(
			reviewUi.countdownMask,
			TweenInfo.new(REVIEW_COUNTDOWN_SECONDS, Enum.EasingStyle.Linear),
			{
				Offset = Vector2.new(-1, 0),
			}
		)
		reviewCountdown.colorTween = TweenService:Create(
			reviewUi.countdownBar,
			TweenInfo.new(REVIEW_COUNTDOWN_SECONDS, Enum.EasingStyle.Linear),
			{ BackgroundColor3 = ERROR_RED }
		)
		reviewCountdown.tween:Play()
		reviewCountdown.colorTween:Play()
		local lastShown = nil
		reviewCountdown.connection = RunService.Heartbeat:Connect(function(dt)
			if not reviewUi or not reviewState or reviewState.decision then
				stopReviewCountdown()
				return
			end
			reviewCountdown.remaining = math.max(reviewCountdown.remaining - dt, 0)
			local seconds = math.ceil(reviewCountdown.remaining)
			if seconds ~= lastShown then
				lastShown = seconds
				reviewUi.countdownLabel.Text = string.format("%s in %ds", label, seconds)
			end
			if reviewCountdown.remaining <= 0 then
				decideReview(decision)
			end
		end)
	end

	function ui.requestEditorPushReview(params, runtimeSettings, helpers)
		local changeCount = tonumber(params.changeCount) or 0
		local threshold = tonumber(runtimeSettings.changesThreshold) or 5
		if
			tostring(runtimeSettings.displayPrompts or "") == "never"
			or changeCount <= threshold
			or ui._playModeHidden
		then
			return { required = false }
		end
		local rows = params.rows
		if type(rows) ~= "table" or #rows == 0 then
			return { required = false }
		end
		local summaryRows = {}
		local groups = {}
		local groupOrder = {}
		local seenServices = {}
		local serviceList = {}
		for _, row in ipairs(rows) do
			if type(row) == "table" then
				local service = tostring(row.service or "")
				if service ~= "" and not seenServices[service] then
					seenServices[service] = true
					table.insert(serviceList, service)
				end
				if row.kind == "instances" then
					table.insert(summaryRows, row)
				elseif type(row.pathSegments) == "table" and #row.pathSegments > 0 then
					local pathKey = table.concat(row.pathSegments, "\1")
					local key = if type(row.pathOrdinals) == "table" and #row.pathOrdinals > 0
						then pathKey .. "\2" .. table.concat(row.pathOrdinals, ",")
						else pathKey
					local group = groups[key]
					if group == nil then
						group = {
							service = service,
							settingsId = row.settingsId,
							pathSegments = row.pathSegments,
							pathOrdinals = row.pathOrdinals,
							className = tostring(row.className or "Instance"),
							entries = {},
						}
						groups[key] = group
						table.insert(groupOrder, group)
					end
					if type(row.entries) == "table" then
						for _, entry in ipairs(row.entries) do
							table.insert(group.entries, entry)
						end
					else
						table.insert(group.entries, row)
					end
				end
			end
		end
		local tree = buildReviewTree(summaryRows, groupOrder, helpers)
		if tree.effectiveCount <= threshold then
			return { required = false }
		end
		if reviewState and not reviewState.decision then
			error("Another editor review is already pending")
		elseif reviewState then
			rememberFinishedReview(reviewState.id, reviewState.decision)
			reviewState = nil
		end
		reviewCounter = reviewCounter + 1
		local reviewId = string.format("review-%d-%d", os.time(), reviewCounter)
		reviewState = { id = reviewId, decision = nil }
		local panel = ensureReviewUi()
		panel.title.Text = "Editor changes awaiting review"
		panel.applyButton.Text = "Apply"
		panel.skipButton.Text = "Skip batch"
		panel.countdownTrack.Visible = true
		panel.countdownLabel.Visible = true
		local changeWord = tree.effectiveCount == 1 and "change" or "changes"
		local instanceWord = tree.instanceCount == 1 and "instance" or "instances"
		local subtitleText = if tree.instanceCount > 0
			then string.format(
				"%d %s across %d %s in %s.",
				tree.effectiveCount,
				changeWord,
				tree.instanceCount,
				instanceWord,
				escapeRich(table.concat(serviceList, ", "))
			)
			else string.format(
				"%d %s in %s.",
				tree.effectiveCount,
				changeWord,
				escapeRich(table.concat(serviceList, ", "))
			)
		panel.subtitle.Text = subtitleText
			.. string.format(
				' This batch is over your review threshold of <font color="%s">%d</font>.',
				colorHex(WARN_AMBER),
				threshold
			)
		panel.treeRoots = tree.roots
		panel.searchBox.Text = ""
		panel.scroll.CanvasPosition = Vector2.zero
		panel.refreshList()
		panel.widget.Enabled = true
		pcall(function()
			panel.widget:RequestRaise()
		end)
		startReviewCountdown("skip", "Skips")
		if runtimeSettings.notifications ~= false then
			ui.notify(
				"pending-review",
				"Changes are waiting for review",
				subtitleText,
				"Review",
				function()
					panel.widget.Enabled = true
					panel.widget:RequestRaise()
				end,
				true
			)
		end
		return { required = true, reviewId = reviewId, effectiveCount = tree.effectiveCount }
	end

	function ui.requestProtectedWriteReview(params, helpers)
		if type(params) ~= "table" or type(params.rows) ~= "table" or #params.rows == 0 then
			return { required = false }
		end
		local result = ui.requestEditorPushReview({
			changeCount = #params.rows,
			rows = params.rows,
		}, {
			changesThreshold = 0,
			displayPrompts = "always",
			initialSyncPriority = "studio",
		}, helpers)
		if result.required then
			local panel = ensureReviewUi()
			stopReviewCountdown()
			panel.title.Text = "Some changes need Studio to restart"
			panel.subtitle.Text = string.format(
				'<font color="%s">%d</font> read-only %s could not be changed while Studio is open.',
				colorHex(WARN_AMBER),
				result.effectiveCount,
				result.effectiveCount == 1 and "property" or "properties"
			)
			panel.applyButton.Text = "Apply anyway"
			panel.skipButton.Text = "Not now"
			panel.countdownTrack.Visible = true
			panel.countdownLabel.Visible = true
			startReviewCountdown("apply", "Applies anyway")
		end
		return result
	end

	function ui.setEditorPushReviewDecision(params)
		local reviewId = tostring(params.reviewId or "")
		local decision = tostring(params.decision or "apply")
		if decision ~= "apply" and decision ~= "skip" then
			return { ok = false, accepted = false, error = "Decision must be apply or skip" }
		end
		if not reviewState or reviewState.decision then
			return { ok = false, accepted = false, error = "No review is awaiting a decision" }
		end
		if reviewId ~= "" and reviewState.id ~= reviewId then
			return { ok = false, accepted = false, error = "Review id does not match the active review" }
		end
		local activeId = reviewState.id
		decideReview(decision)
		return { ok = true, accepted = true, reviewId = activeId, decision = decision }
	end

	function ui.getEditorPushReviewDecision(params)
		local reviewId = tostring(params.reviewId or "")
		if reviewState and reviewState.id == reviewId then
			if not reviewState.decision then
				return { decided = false }
			end
			local decision = reviewState.decision
			reviewState = nil
			return { decided = true, decision = decision }
		end
		pruneFinishedReviewDecisions()
		local finished = finishedReviewDecisions[reviewId]
		if finished then
			finishedReviewDecisions[reviewId] = nil
			return { decided = true, decision = finished.decision }
		end
		return { decided = true, decision = "skip", error = "Review id was not found" }
	end

	function ui.pendingReviewCount()
		return if reviewState ~= nil and reviewState.decision == nil then 1 else 0
	end

	statusUi.widget:GetPropertyChangedSignal("Enabled"):Connect(function()
		openButton:SetActive(statusUi.widget.Enabled and not ui._playModeHidden)
	end)
	openButton:SetActive(statusUi.widget.Enabled and not ui._playModeHidden)

	openButton.Click:Connect(function()
		ui.toggleWidget()
	end)
	actions.status.Triggered:Connect(ui.showWidget)
	actions.review.Triggered:Connect(function()
		if reviewUi and reviewState and not reviewState.decision then
			reviewUi.widget.Enabled = true
			reviewUi.widget:RequestRaise()
		else
			ui.showWidget()
			ui.notify("no-review", "No review is waiting", "Renium has no pending changes to review.", nil, nil, false)
		end
	end)

	function ui.updateStatus(view)
		ui._lastView = view
		local mode = tostring(view.mode or "disconnected")
		local color = if mode == "connected"
			then OK_GREEN
			elseif mode == "connecting"
			then WARN_AMBER
			elseif string.find(tostring(view.connectionStatus or ""), "interrupted", 1, true)
			then ERROR_RED
			else IDLE_GREY

		statusUi.dot.BackgroundColor3 = color
		statusUi.statusTitle.Text = tostring(view.title or "Disconnected")
		local subtitle = tostring(view.subtitle or "")
		statusUi.statusSubtitle.Text = if subtitle == "" then " " else subtitle
		local syncText = tostring(view.syncText or "")
		statusUi.syncLine.Text = syncText

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
		if reviewUi then
			applyThemeToRefs(reviewUi.refs, p)
			reviewUi.refreshList()
		end
		settingsUi.paintSegments()
		if ui._lastView then
			ui.updateStatus(ui._lastView)
		end
	end

	ui.applyStudioTheme()
	settings().Studio.ThemeChanged:Connect(ui.applyStudioTheme)

	return ui
end

return BridgeUi
