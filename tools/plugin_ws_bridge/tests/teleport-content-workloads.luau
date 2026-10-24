local fs = require("@lune/fs")
local luau = require("@lune/luau")

local function node(class, name, children)
	local object = { ClassName = class, Name = name, children = children or {}, attributes = {} }
	function object:IsA(expected) return expected == self.ClassName end
	function object:GetChildren() return self.children end
	function object:GetDescendants()
		local result = {}
		for _, child in self.children do
			result[#result + 1] = child
			for _, descendant in child:GetDescendants() do result[#result + 1] = descendant end
		end
		return result
	end
	function object:GetAttribute(key) return self.attributes[key] end
	function object:GetAttributes() return table.clone(self.attributes) end
	function object:GetTags() return {} end
	function object:FindFirstChild(childName)
		for _, child in self.children do if child.Name == childName then return child end end
		return nil
	end
	return object
end

local workspace = node("Workspace", "Workspace")
local lighting = node("Lighting", "Lighting")
lighting.Brightness = 1
lighting.ReadOnlyMode = "unchanged"
local services = { Workspace = workspace, Lighting = lighting, TextChatService = node("TextChatService", "TextChatService") }
local content = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeTeleportContent.module.luau"), { environment = {
	workspace = workspace, game = { GetService = function(_, service) return assert(services[service], service) end },
} })()
local script = node("Script", "DestinationScript")
script.Disabled = true
script.attributes.__ReniumTeleportRef = "script"
script.attributes.__ReniumTeleportDisabled = false
local reference = node("ObjectValue", "Reference")
reference.attributes.__ReniumTeleportRef = "reference"
local group = node("Folder", "payload", { script, reference })
local codec = { decode = function(value) return true, value end }
local collisions = { decode = function(value) assert(value == "collision-data"); return {} end }
local manifest = { checks = {}, groups = { { name = "payload", path = { "Lighting" }, properties = {
	{ name = "Brightness", value = 2, write = true }, { name = "ReadOnlyMode", value = "unchanged", write = false },
} } }, references = { { id = "reference", property = "Value", path = { "Workspace" } } } }
local prepared = content.prepare({ group }, manifest, codec, collisions)
assert(#prepared == 1 and prepared[1].properties[1].value == 2)
assert(lighting.Brightness == 1 and script.Disabled, "preparation must not modify live services or run destination scripts")
assert(reference.Value == workspace, "service references bind to existing engine services")
script.Disabled = false
assert(not pcall(content.prepare, { group }, manifest, codec, collisions))
script.Disabled = true
manifest.groups[1].properties[2].value = "different"
assert(not pcall(content.prepare, { group }, manifest, codec, collisions))
manifest.groups[1].properties[2].value = "unchanged"
manifest.references[1].id = "missing"
assert(not pcall(content.prepare, { group }, manifest, codec, collisions))
manifest.references[1].id = "reference"
for _, class in { "Terrain", "PackageLink" } do
	script.ClassName = class
	assert(not pcall(content.prepare, { group }, manifest, codec, collisions))
end
script.ClassName = "Script"
manifest.groups[1].path = { "CoreGui" }
assert(not pcall(content.prepare, { group }, manifest, codec, collisions))
manifest.groups[1].path = { "Workspace" }
manifest.groups[1].properties = { { name = "CollisionGroupData", value = "collision-data", write = true } }
assert(content.prepare({ group }, manifest, codec, collisions)[1].properties[1].collisionGroups == collisions)
manifest.groups[1].properties[1].value = "invalid"
assert(not pcall(content.prepare, { group }, manifest, codec, collisions))
manifest.groups[1].properties = {}
manifest.checks = { { path = { "TextChatService" }, class = "TextChatService", properties = {} } }
assert(content.prepare({ group }, manifest, codec, collisions))
services.TextChatService.attributes.changed = true
assert(not pcall(content.prepare, { group }, manifest, codec, collisions))
services.TimerService = node("TimerService", "TimerService")
manifest.checks = { { path = { "TimerService" }, class = "TimerService", properties = {}, empty = true } }
assert(content.prepare({ group }, manifest, codec, collisions), "empty saved engine services can be retained")
table.insert(services.TimerService.children, node("Folder", "SourceOnly"))
assert(not pcall(content.prepare, { group }, manifest, codec, collisions), "source content must not silently survive")
assert(lighting.Brightness == 1 and script.Disabled, "failed preflights preserve source and disabled scripts")
print("teleport content preflight workloads passed")
