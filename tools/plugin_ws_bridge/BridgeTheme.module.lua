local BridgeTheme = {}

local function setBackground(instance, color)
	if instance ~= nil then
		instance.BackgroundColor3 = color
	end
end

local function setText(instance, color)
	if instance ~= nil then
		instance.TextColor3 = color
	end
end

local function setStroke(instance, color)
	if instance ~= nil then
		instance.Color = color
	end
end

local function setImageColor(instance, color)
	if instance ~= nil then
		instance.ImageColor3 = color
	end
end

local function applyList(list, callback, color)
	for _, instance in ipairs(list or {}) do
		callback(instance, color)
	end
end

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

function BridgeTheme.apply(theme, refs)
	local mainBackground = studioColor(theme, "MainBackground", Color3.fromRGB(31, 31, 31))
	local panelBackground = studioColor(theme, "Titlebar", Color3.fromRGB(40, 40, 40))
	local borderColor = studioColor(theme, "Border", Color3.fromRGB(72, 72, 72))
	local textColor = studioColor(theme, "MainText", Color3.fromRGB(235, 235, 235))
	local subTextColor = studioColor(theme, "DimmedText", Color3.fromRGB(170, 170, 170))
	local inputColor = studioColor(theme, "InputFieldBackground", panelBackground)
	local buttonColor = studioColor(theme, "Button", Color3.fromRGB(58, 58, 58))
	local buttonTextColor = studioColor(theme, "ButtonText", textColor)
	local primaryColor = studioColor(theme, "DialogMainButton", Color3.fromRGB(0, 120, 215))
	local primaryTextColor = studioColor(theme, "DialogMainButtonText", Color3.fromRGB(255, 255, 255))

	setBackground(refs.rootFrame, mainBackground)
	applyList(refs.panelFrames, setBackground, panelBackground)
	applyList(refs.inputFrames, setBackground, inputColor)
	applyList(refs.strokes, setStroke, borderColor)
	applyList(refs.textLabels, setText, textColor)
	applyList(refs.subTextLabels, setText, subTextColor)
	applyList(refs.logoImages, setImageColor, textColor)

	setBackground(refs.hostBox, inputColor)
	setBackground(refs.portsBox, inputColor)
	setText(refs.hostBox, textColor)
	setText(refs.portsBox, textColor)
	refs.hostBox.PlaceholderColor3 = subTextColor
	refs.portsBox.PlaceholderColor3 = subTextColor

	setBackground(refs.connectButton, primaryColor)
	setText(refs.connectButton, primaryTextColor)
	setBackground(refs.disconnectButton, buttonColor)
	setText(refs.disconnectButton, buttonTextColor)
end

return BridgeTheme
