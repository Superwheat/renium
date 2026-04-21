local BridgeTheme = {}

function BridgeTheme.apply(theme, refs)
	refs.rootFrame.BackgroundColor3 = theme:GetColor(Enum.StudioStyleGuideColor.MainBackground)

	local panelColor = theme:GetColor(Enum.StudioStyleGuideColor.Titlebar)
	refs.hostBox.BackgroundColor3 = panelColor
	refs.portsBox.BackgroundColor3 = panelColor
	refs.exportAllButton.BackgroundColor3 = panelColor
	refs.preSerializeButton.BackgroundColor3 = panelColor
	refs.applyButton.BackgroundColor3 = theme:GetColor(Enum.StudioStyleGuideColor.DialogMainButton)
	refs.applyButton.TextColor3 = theme:GetColor(Enum.StudioStyleGuideColor.DialogMainButtonText)

	local textColor = theme:GetColor(Enum.StudioStyleGuideColor.MainText)
	refs.hostLabel.TextColor3 = textColor
	refs.portsLabel.TextColor3 = textColor
	refs.hostBox.TextColor3 = textColor
	refs.portsBox.TextColor3 = textColor
	refs.exportAllButton.TextColor3 = textColor
	refs.preSerializeButton.TextColor3 = textColor
	refs.statusLabel.TextColor3 = textColor
end

return BridgeTheme
