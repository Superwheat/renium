local echo = Instance.new("RemoteFunction")
echo.Name = "ReniumNetworkEcho"
echo.OnServerInvoke = function()
	return true
end
echo.Parent = game:GetService("ReplicatedStorage")
