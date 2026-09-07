-- Own only this temporary model; retain the fixture's original geometry.
assert(game.PlaceId == 0 and game:GetService("RunService"):IsEdit())
assert(workspace:FindFirstChild("ReniumSerializerProbe") == nil)
local root = Instance.new("Model")
root.Name = "ReniumSerializerProbe"
local original = workspace.ReniumAccessFixture
assert(original:IsA("MeshPart") and original.MeshId ~= "")
local mesh = original:Clone()
mesh.Name = "Mesh"
mesh.Anchored = true
mesh.Size = Vector3.new(3, 4, 5)
mesh.CFrame = CFrame.new(17, 13, -9)
mesh.Archivable = false
mesh.Parent = root
local reference = Instance.new("ObjectValue")
reference.Name = "Pointer"
reference.Value = mesh
reference.Parent = root
root.WorldPivot = CFrame.new(31, 19, -11)
root.Parent = workspace
return mesh:GetDebugId()
