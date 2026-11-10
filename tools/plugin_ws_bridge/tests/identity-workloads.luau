local fs = require("@lune/fs")
local luau = require("@lune/luau")
local mockGame = {}
local Identity = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeIdentity.module.luau"), {
	environment = { game = mockGame },
})()

for _, count in { 1000, 2000, 4000 } do
	local children = {}
	local calls = 0
	local service = { Name = "Workspace", Parent = mockGame, IsDescendantOf = function() return true end }
	local parent = { Name = "Folder", Parent = service, IsDescendantOf = function() return true end }
	function service:GetChildren() return { parent } end
	function parent:GetChildren()
		calls += 1
		return children
	end
	for index = 1, count do
		children[index] = { Name = "Duplicate", Parent = parent, IsDescendantOf = function() return true end }
	end
	local snapshot = Identity.newPathSnapshot()
	for index, instance in children do
		local segments, ordinals = Identity.getRefPathParts(instance, snapshot)
		assert(segments[3] == "Duplicate" and ordinals[3] == index)
	end
	assert(calls == 1, `Expected one sibling traversal, got {calls}`)
	children[1].Name = "Renamed"
	local _, ordinals = Identity.getRefPathParts(children[2], Identity.newPathSnapshot())
	assert(ordinals[3] == 1, "A fresh snapshot must see renamed siblings")
	local mutableState = { pathSegmentsByInstance = {}, pathOrdinalsByInstance = {} }
	Identity.getCachedRefPathParts(mutableState, children[2])
	children[3] = { Name = "Duplicate", Parent = parent, IsDescendantOf = function() return true end }
	local _, replacementOrdinals = Identity.getCachedRefPathParts(mutableState, children[3])
	assert(replacementOrdinals[3] == 2, "Mutable service state must resolve replacement ordinals afresh")
	assert(mutableState.siblingOrdinalsByParent == nil, "Only fixed snapshots own sibling indexes")
end
do
	local calls = 0
	local children = {}
	local service = { Name = "Workspace" }
	local parent = { Name = "Folder" }
	function service:FindFirstChild(name) return if name == parent.Name then parent else nil end
	function parent:GetChildren()
		calls += 1
		return children
	end
	function parent:FindFirstChild(name)
		for _, child in children do
			if child.Name == name then
				return child
			end
		end
		return nil
	end
	for index = 1, 300 do
		children[index] = { Name = if index % 3 == 0 then "Other" else "Duplicate" }
	end
	mockGame.GetService = function(_, name)
		assert(name == "Workspace")
		return service
	end
	local segments = { "Workspace", "Folder", "Duplicate" }
	local expected = {}
	for ordinal = 1, 201 do
		expected[ordinal] = Identity.resolvePathSegments(segments, nil, { 1, 1, ordinal })
	end
	assert(expected[200] ~= nil and expected[201] == nil)
	calls = 0
	local childIndex = Identity.newChildIndex()
	for ordinal = 1, 201 do
		local resolved = Identity.resolvePathSegments(segments, nil, { 1, 1, ordinal }, childIndex)
		assert(resolved == expected[ordinal], `Indexed ordinal {ordinal} resolved a different child`)
	end
	assert(calls == 1, `Expected one child scan with an index, got {calls}`)
	mockGame.GetService = nil
end
do
	local function alive() return true end
	local service = { Name = "Vault", ClassName = "ServerStorage", Parent = mockGame, IsDescendantOf = alive }
	local namesake = { Name = "ServerStorage", ClassName = "Folder", Parent = mockGame, IsDescendantOf = alive }
	local child = { Name = "Item", ClassName = "Part", Parent = service, IsDescendantOf = alive }
	local stray = { Name = "Item", ClassName = "Part", Parent = namesake, IsDescendantOf = alive }
	function service:GetChildren() return { child } end
	function service:FindFirstChild(name) return if name == child.Name then child else nil end
	function namesake:GetChildren() return { stray } end
	mockGame.FindService = function(_, className)
		if className == "ServerStorage" then
			return service
		end
		error(`'{className}' is not a valid Service name`)
	end
	mockGame.GetService = function(_, name)
		assert(name == "ServerStorage", `A path root must be a service class name, got {name}`)
		return service
	end
	for _, snapshot in { false, Identity.newPathSnapshot() } do
		local segments, ordinals = Identity.getRefPathParts(child, snapshot or nil)
		assert(segments[1] == "ServerStorage" and segments[2] == "Item", "A renamed service is addressed by class name")
		assert(Identity.resolvePathSegments(segments, nil, ordinals) == child, "The path of a renamed service's child resolves back")
		local rootSegments = Identity.getRefPathParts(service, snapshot or nil)
		assert(#rootSegments == 1 and rootSegments[1] == "ServerStorage", "A renamed service's own path is its class name")
		local straySegments = Identity.getRefPathParts(namesake, snapshot or nil)
		assert(straySegments[1] == "ServerStorage", "A root that is no service keeps its Name")
	end
	local reference = Identity.serializeRefValue({
		instanceIdByInstance = {}, pathSegmentsByInstance = {}, pathOrdinalsByInstance = {}, debugIdByInstance = { [child] = false },
	}, child)
	assert(reference.pathSegments[1] == "ServerStorage", "A reference into a renamed service starts with its class name")
	local keyed = { pathByInstance = {} }
	assert(Identity.getCachedInstancePath(keyed, child) == "ServerStorage.Item", "A dotted path into a renamed service starts with its class name")
	assert(Identity.getCachedInstancePath(keyed, stray) == "ServerStorage.Item" and keyed.pathByInstance[namesake] == "ServerStorage")
	mockGame.FindService = nil
	mockGame.GetService = nil
end
print("Identity workload and duplicate ordinal tests passed")
