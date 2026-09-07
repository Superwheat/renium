assert(game.PlaceId == 0, "Disposable local place required")
assert(game:GetService("RunService"):IsEdit(), "Edit mode required")
assert(not workspace:FindFirstChild("ReniumFinalSync"), "Probe already exists")
local root = Instance.new("Folder")
root.Name = "ReniumFinalSync"
root:SetAttribute("Revision", 0)
root.Parent = workspace
local target = Instance.new("Part")
target.Name = "Target"
target.Anchored = true
target.Size = Vector3.new(2, 3, 4)
target:SetAttribute("Marker", "keep")
target.Parent = root
local pointer = Instance.new("ObjectValue")
pointer.Name = "Pointer"
pointer.Value = target
pointer.Parent = root
for index = 1, 2 do
	local duplicate = Instance.new("StringValue")
	duplicate.Name = "Duplicate"
	duplicate.Value = `duplicate-{index}`
	duplicate.Parent = root
end
local module = Instance.new("ModuleScript")
module.Name = "ReniumFinalSource"
module.Source = "return { revision = 0 }\n"
module.Parent = game:GetService("ReplicatedStorage")
local gui = Instance.new("ScreenGui")
gui.Name = "ReniumFinalGui"
gui.Parent = game:GetService("StarterGui")
local frame = Instance.new("Frame")
frame.Name = "Frame"
frame.Parent = gui
local corner = Instance.new("UICorner")
corner.CornerRadius = UDim.new(0.25, 3)
corner.Parent = frame
return true
