local fs = require("@lune/fs")
local luau = require("@lune/luau")
local serde = require("@lune/serde")

-- Small instance graph: exercise the real transplant's parenting and cleanup.
local function node(class, name, parent)
	local properties = { ClassName = class, Name = name or class, Archivable = true }
	local children, attributes, tags, methods = {}, {}, {}, {}
	local self
	function methods:IsA(kind) return properties.ClassName == kind end
	function methods:GetChildren() return table.clone(children) end
	function methods:GetDescendants()
		local result = {}
		for _, child in children do
			table.insert(result, child)
			for _, descendant in child:GetDescendants() do table.insert(result, descendant) end
		end
		return result
	end
	function methods:FindFirstChild(childName)
		for _, child in children do if child.Name == childName then return child end end
		return nil
	end
	function methods:GetAttributes() return table.clone(attributes) end
	function methods:GetAttribute(key) return attributes[key] end
	function methods:SetAttribute(key, value) attributes[key] = value end
	function methods:GetTags() return table.clone(tags) end
	function methods:AddTag(tag) table.insert(tags, tag) end
	function methods:RemoveTag(tag) table.remove(tags, assert(table.find(tags, tag))) end
	function methods:Clear() properties.cleared = true end
	function methods:ClearAllChildren()
		for _, child in self:GetChildren() do child:Destroy() end
	end
	function methods:Destroy() self:ClearAllChildren(); self.Parent = nil; properties.destroyed = true end
	function methods:Clone()
		local copy = node(class, properties.Name)
		for key, value in properties do if key ~= "Parent" then copy[key] = value end end
		for key, value in attributes do copy:SetAttribute(key, value) end
		for _, child in children do child:Clone().Parent = copy end
		return copy
	end
	self = setmetatable({ _children = children }, {
		__index = function(_, key) return methods[key] or properties[key] or methods.FindFirstChild(self, key) end,
		__newindex = function(_, key, value)
			if key == "Parent" then
				if properties.Parent then table.remove(properties.Parent._children, assert(table.find(properties.Parent._children, self))) end
				if value then table.insert(value._children, self) end
			end
			properties[key] = value
		end,
	})
	self.Parent = parent
	return self
end

local services = {}
for _, service in { "Workspace", "Lighting", "ReplicatedFirst", "ReplicatedStorage", "ServerScriptService",
	"ServerStorage", "StarterGui", "StarterPack", "StarterPlayer", "Teams", "SoundService" } do
	services[service] = node(service)
end
node("StarterPlayerScripts", nil, services.StarterPlayer)
node("StarterCharacterScripts", nil, services.StarterPlayer)
local camera = node("Camera", nil, services.Workspace)
services.Workspace.CurrentCamera = camera
local terrain = node("Terrain", nil, services.Workspace)
local player = node("Player")
node("PlayerScripts", nil, player)
node("PlayerGui", nil, player)
node("Backpack", nil, player)
services.Players = { GetPlayers = function() return {} end, LocalPlayer = player }
services.HttpService = { JSONEncode = function(_, value) return serde.encode("json", value) end }
local content = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeTeleportContent.module.luau"), { environment = {
	Instance = { new = node }, workspace = services.Workspace,
	game = { PlaceId = 11, GameId = 7, JobId = "", GetService = function(_, name) return assert(services[name], name) end },
} })()
local request = function() return 17 end
local shim = content.installShim(node("ModuleScript"), request)
assert(shim.Parent == services.ReplicatedStorage and shim.Request.OnInvoke() == 17)
assert(serde.decode("json", shim.Context.Value).mode == "source")
assert(not pcall(content.installShim, node("ModuleScript"), request), "a reserved-name collision must not overwrite content")
local contextIdentity = shim.Context
local nativeRelay = node("BindableFunction", "__ReniumTeleportNative_owned", services.ServerStorage)
nativeRelay.OnInvoke = request
for attempt = 1, 2 do
	local sourcePart = node("Part", "Source", services.Workspace)
	local sourceModule = node("ModuleScript", "Source", services.ReplicatedStorage)
	local sourceRelay = node("BindableFunction", nativeRelay.Name, services.ServerStorage)
	local incoming = node("Folder", "workspace")
	local part = node("Part", "Destination", incoming)
	local storage = node("Folder", "storage")
	local module = node("ModuleScript", "Destination", storage)
	local serverStorage = node("Folder", "serverStorage")
	incoming:SetAttribute("DestinationAttribute", attempt)
	incoming:AddTag("DestinationTag")
	local groups = { { root = incoming, target = services.Workspace, properties = {} },
		{ root = storage, target = services.ReplicatedStorage, properties = {} },
		{ root = serverStorage, target = services.ServerStorage, properties = {} } }
	local bundle = content.stageClients(groups, { id = attempt, placeId = 12, data = {} }, node("ModuleScript"))
	assert(bundle.ServiceMetadata.workspace:GetAttribute("DestinationAttribute") == attempt)
	assert(bundle.ServiceMetadata.workspace:GetTags()[1] == "DestinationTag")
	for _, name in { "PlayerScripts", "Gui", "First" } do node("Folder", name, bundle) end
	local count = content.commit(groups, { id = attempt }, bundle, function() end, shim, nativeRelay)
	assert(sourcePart.destroyed and sourceModule.destroyed)
	assert(sourceRelay.destroyed and not nativeRelay.destroyed and nativeRelay.Parent == services.ServerStorage
		and nativeRelay.OnInvoke() == 17, "retain only the owned native relay across compatibility hops")
	assert(part.Parent == services.Workspace and module.Parent == services.ReplicatedStorage)
	assert(camera.Parent == services.Workspace and terrain.cleared)
	assert(shim.Parent == services.ReplicatedStorage and shim.Request.OnInvoke() == 17)
	assert(shim.Context == contextIdentity and shim:GetAttribute("Attempt") == attempt)
	assert(count == #shim:GetDescendants() and serde.decode("json", shim.Context.Value).id == attempt)
end
local gui = node("ScreenGui", "Loading", player.PlayerGui)
local stale = {}
for _, service in { services.Workspace, services.Lighting, services.SoundService, services.ReplicatedFirst,
	services.ReplicatedStorage, services.StarterGui, services.StarterPack, services.StarterPlayer.StarterPlayerScripts,
	player.PlayerGui, player.PlayerScripts, player.Backpack } do
	table.insert(stale, node("Folder", "ClientSource", service))
end
local engineController = node("ModuleScript", "PlayerModule", player.PlayerScripts)
services.Lighting.Brightness = 99
services.Lighting:SetAttribute("ClientOnly", true)
services.Lighting:AddTag("ClientOnly")
local clientSettings = content.prepareClient({ { name = "lighting", path = { "Lighting" }, properties = {
	{ name = "Brightness", value = 2, write = true },
} } }, { decode = function(value) return true, value end }, {})
content.clearClient(gui, clientSettings)
assert(services.Lighting.Brightness == 2 and next(services.Lighting:GetAttributes()) == nil and #services.Lighting:GetTags() == 0,
	"source-only client service properties and metadata must not survive")
for _, child in stale do assert(child.destroyed, "client source content survived in Explorer") end
assert(gui.Parent == player.PlayerGui and engineController.Parent == player.PlayerScripts)
assert(shim.Parent == services.ReplicatedStorage and camera.Parent == services.Workspace)
local lightingMetadata = node("Folder", "lighting", shim.ServiceMetadata)
lightingMetadata:SetAttribute("DestinationAttribute", 2)
lightingMetadata:AddTag("DestinationTag")
local replicatedCount = #shim:GetDescendants()
content.arriveClient({ id = 2, replicatedCount = replicatedCount }, gui, function() end, clientSettings)
assert(services.Lighting:GetAttribute("DestinationAttribute") == 2 and services.Lighting:GetAttribute("ClientOnly") == nil)
assert(services.Lighting:GetTags()[1] == "DestinationTag" and shim.ArrivingGui.Value == gui)
local previousArrival = shim.ArrivingGui
content.clearClient(gui, clientSettings)
assert(previousArrival.destroyed and not shim:FindFirstChild("ArrivingGui"), "a client-only receipt must not block the next hop")
assert(#shim:GetDescendants() == replicatedCount, "the receipt must match the server count after client cleanup")
shim:SetAttribute("Attempt", 3)
content.arriveClient({ id = 3, replicatedCount = replicatedCount }, gui, function() end, clientSettings)
assert(shim.ArrivingGui ~= previousArrival and shim.ArrivingGui.Value == gui)
print("teleport transplant identity and client Explorer cleanup workloads passed")
