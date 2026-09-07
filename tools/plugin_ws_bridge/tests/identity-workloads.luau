local fs = require("@lune/fs")
local luau = require("@lune/luau")
local mockGame = {}
local Identity = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeIdentity.module.lua"), {
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
print("Identity workload and duplicate ordinal tests passed")
