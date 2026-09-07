local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")

local function signal()
	local callbacks = {}
	return {
		Count = function() local count = 0 for _ in callbacks do count += 1 end return count end,
		Connect = function(_, callback)
			callbacks[callback] = true
			return { Disconnect = function() callbacks[callback] = nil end }
		end,
		Fire = function(_, ...)
			for callback in callbacks do callback(...) end
		end,
	}
end

local function instance(className, name, parent)
	local item = { ClassName = className, Name = name, Parent = parent, Archivable = true, children = {} }
	if parent then table.insert(parent.children, item) end
	for _, name in { "Changed", "AncestryChanged", "AttributeChanged", "DescendantAdded", "DescendantRemoving" } do
		item[name] = signal()
	end
	function item:IsA(class) return self.ClassName == class or class == "Instance" or class == "ValueBase" and self.ClassName == "StringValue" end
	local propertySignals = {}
	function item:GetPropertyChangedSignal(property)
		if not propertySignals[property] then propertySignals[property] = signal() end
		return propertySignals[property]
	end
	function item:IsDescendantOf(ancestor)
		local cursor = self.Parent
		while cursor do
			if cursor == ancestor then return true end
			cursor = cursor.Parent
		end
		return false
	end
	function item:GetChildren() return self.children end
	function item:GetDescendants()
		local out = {}
		for _, child in self.children do
			table.insert(out, child)
			for _, descendant in child:GetDescendants() do table.insert(out, descendant) end
		end
		return out
	end
	function item:GetAttributes() return {} end
	function item:GetFullName() return if self.Parent then self.Parent:GetFullName() .. "." .. self.Name else self.Name end
	return item
end

local testGame = instance("DataModel", "game")
local storage = instance("ReplicatedStorage", "ReplicatedStorage", testGame)
local workspace = instance("Workspace", "Workspace", testGame)
local corner = instance("UICorner", "Corner", storage)
local left = instance("Folder", "Left", storage)
local right = instance("Folder", "Right", storage)
local moving = instance("Part", "Duplicate", left)
local sibling = instance("Part", "Duplicate", left)
local value = instance("StringValue", "ValueDuplicate", left)
local valueSibling = instance("StringValue", "ValueDuplicate", left)
local mesh = instance("MeshPart", "Mesh", storage)
mesh.CollisionFidelity = "Box"
local names = { "TopLeftRadius", "TopRightRadius", "BottomLeftRadius", "BottomRightRadius" }
for _, name in names do corner[name] = roblox.UDim.new(0, 8) end
local services = {
	ReplicatedStorage = storage, Workspace = workspace, EncodingService = {},
	CollectionService = { GetTags = function() return {} end, GetAllTags = function() return {} end },
}
function testGame:GetService(name) return assert(services[name], name) end
local codec = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeValueCodec.module.lua"), { environment = { game = testGame } })()
local dependencies = { BridgeScriptDocuments = {}, BridgeValueCodec = codec,
	RbxDom = { findCanonicalPropertyDescriptor = function() return { dataType = "UDim" } end } }
local deferred = nil
local function flushDeferred()
	while #deferred > 0 do
		local callbacks = deferred
		deferred = {}
		for _, callback in callbacks do callback() end
	end
end
local trackerModule = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeStudioChanges.module.lua"), {
	environment = {
		game = testGame,
		typeof = function(value) return if type(value) == "table" and value.ClassName then "Instance" else typeof(value) end,
		script = { Parent = dependencies }, require = function(module) return module end,
		Instance = { new = function(class)
			assert(class == "BindableEvent")
			local event = signal()
			return { Event = event, Fire = function(_, ...) event:Fire(...) end, Destroy = function() end }
		end },
		task = { delay = function() end, spawn = function() end, defer = function(callback)
			if deferred then table.insert(deferred, callback) else callback() end
		end },
	},
})()
local trackerConfig = {
	bridgeRole = "edit", bridgeRuntimeId = "test", LUA_SOURCE_CLASS = {},
	loadPendingStudioChanges = function() end, savePendingStudioChanges = function() end,
	shouldIgnoreInstance = function() return false end, invalidateRuntimeExportCache = function() end,
}
local tracker = trackerModule.create(trackerConfig, { ReplicatedStorage = true, Workspace = true })
local candidates = table.clone(names)
table.insert(candidates, "CornerRadius")
tracker.configurePropertyCandidates({ UICorner = candidates, Part = { "Transparency" }, StringValue = { "Value" }, MeshPart = { "CollisionFidelity" } })
tracker.getState({ services = { "ReplicatedStorage" } })
assert(moving.Changed:Count() == 1 and moving.AttributeChanged:Count() == 1 and moving.AncestryChanged:Count() == 0)
assert(value:GetPropertyChangedSignal("Parent"):Count() == 1)
tracker.beginSuppress()
tracker.beginChangeJournal("corner", { "ReplicatedStorage" })
local generation = tracker.serviceGeneration("ReplicatedStorage")
local function change(name, value, aliasFirst)
	corner[name] = value
	if name == "TopLeftRadius" and aliasFirst then corner.Changed:Fire("CornerRadius") end
	corner.Changed:Fire(name)
	if name == "TopLeftRadius" and not aliasFirst then corner.Changed:Fire("CornerRadius") end
end
for index = 1, 200 do
	local value = roblox.UDim.new(index / 200, 0)
	tracker.expectPropertyEvent(corner, "TopLeftRadius", value)
	change("TopLeftRadius", value, index % 2 == 0)
	assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "own coupled property event became a Studio edit")
end
assert(#tracker.drainChangeJournal("corner") == 0)
local all = roblox.UDim.new(0.25, 3)
tracker.expectPropertyEvent(corner, "CornerRadius", all)
for _, name in names do change(name, all, true) end
assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "legacy write must expect all four radii")
local external = roblox.UDim.new(0.5, 7)
local token = tracker.expectPropertyEvent(corner, "CornerRadius", external)
tracker.cancelExpectedEvent(token)
change("TopLeftRadius", external, true)
assert(tracker.serviceGeneration("ReplicatedStorage") == generation + 1, "real edit must still conflict")
local journal = tracker.drainChangeJournal("corner")
assert(#journal == 1 and journal[1].properties.TopLeftRadius.value == external)
assert(journal[1].properties.CornerRadius == nil, "journal must not replay the legacy all-corners setter")
local function changeAndReadOrdinal(target, index)
	if target:IsA("ValueBase") then target.Value = tostring(index) target.Changed:Fire(target.Value)
	else target.Transparency = index / 1000 target.Changed:Fire("Transparency") end
	local records = tracker.drainChangeJournal("corner")
	for _, record in records do
		if record.instance == target then return record.pathOrdinals[#record.pathOrdinals] end
	end
	error("missing property change")
end
local function move(target, parent)
	table.remove(target.Parent.children, assert(table.find(target.Parent.children, target)))
	table.insert(parent.children, target)
	target.Parent = parent
	if target:IsA("ValueBase") then target:GetPropertyChangedSignal("Parent"):Fire()
	else target.Changed:Fire("Parent") end
	tracker.drainChangeJournal("corner")
end
for index = 1, 100 do
	assert(changeAndReadOrdinal(sibling, index * 2) == 2)
	move(moving, right)
	assert(changeAndReadOrdinal(sibling, index * 2 + 1) == 1, "source sibling ordinal stayed stale")
	move(moving, left)
	-- Put it back in front, firing the same two parent transitions used by Studio.
	move(sibling, right) move(sibling, left)
	-- ValueBase never emits Changed("Parent"). Its narrow signal must invalidate both parents.
	local before = changeAndReadOrdinal(valueSibling, index * 2)
	move(value, right)
	assert(changeAndReadOrdinal(valueSibling, index * 2 + 1) == before - 1)
	move(value, left)
	move(valueSibling, right) move(valueSibling, left)
end
tracker.endSuppress(0)

-- Use the production tracker for net structural changes, including deferred
-- removal callbacks which run after Parent has already become nil in Studio.
local function resetPending()
	tracker.getState({ reset = true })
end
local function add(target, parent)
	target.Parent = parent
	table.insert(parent.children, target)
	local service = if target:IsDescendantOf(storage) then storage else workspace
	service.DescendantAdded:Fire(target)
	for _, descendant in target:GetDescendants() do service.DescendantAdded:Fire(descendant) end
end
local function remove(target)
	local descendants = target:GetDescendants()
	local service = if target:IsDescendantOf(storage) then storage else workspace
	table.remove(target.Parent.children, assert(table.find(target.Parent.children, target)))
	target.Parent = nil
	-- Fire Parent while the tracker is still connected, as happens with deferred
	-- Studio signals; the structural event is authoritative for leaving a service.
	target.Changed:Fire("Parent")
	service.DescendantRemoving:Fire(target)
	for _, descendant in descendants do service.DescendantRemoving:Fire(descendant) end
end
local function pending(expected, label)
	assert(tracker.pendingChangeCount() == expected, label .. ": UI count")
	local state = tracker.getState({})
	assert(state.changeCount == expected, label .. ": command count")
	assert((#state.dirtyServices == 0) == (expected == 0), label .. ": dirty service")
	assert((#state.fullSyncServices == 0) == (expected == 0), label .. ": full sync service")
	for _, entry in state.changes do assert(entry.instance == nil and entry.firstSeq == nil) end
	return state
end
resetPending()
local before = tracker.serviceGeneration("ReplicatedStorage")
for _ = 1, 200 do
	local root = instance("Folder", "Temporary")
	for _ = 1, 75 do instance("Folder", "Duplicate", root) end
	add(root, storage)
	root.Name = "Renamed"
	root.Changed:Fire("Name")
	remove(root)
	pending(0, "new subtree deleted before sync")
end
assert(tracker.serviceGeneration("ReplicatedStorage") > before, "cancellation must not rewind concurrency generation")

resetPending()
for _ = 1, 1200 do
	local root = instance("Folder", "Burst")
	add(root, storage)
	remove(root)
end
local survivor = instance("Folder", "Survivor")
add(survivor, storage)
pending(1, "rapid cancellation cannot overflow retention or erase the next addition")
remove(survivor)
pending(0, "rapid cancellation cleanup")

resetPending()
local temporary = instance("Folder", "Temporary")
add(temporary, storage)
local captured = tracker.getState({})
remove(temporary)
pending(0, "captured but unacknowledged addition removed")
tracker.getState({ ackSeq = captured.seq, runtimeId = "test" })
assert(tracker.pendingChangeCount() > 0, "late addition acknowledgment must retain the deletion")
local deleted = tracker.getState({})
tracker.getState({ ackSeq = deleted.seq, runtimeId = "test" })
pending(0, "deletion acknowledged")

resetPending()
local existing = instance("Folder", "Existing")
add(existing, storage)
local saved = tracker.getState({})
tracker.getState({ ackSeq = saved.seq, runtimeId = "test" })
remove(existing)
assert(tracker.pendingChangeCount() > 0, "deleting a saved object must stay pending")

resetPending()
add(existing, storage)
saved = tracker.getState({})
tracker.getState({ ackSeq = saved.seq, runtimeId = "test" })
remove(existing)
add(existing, right)
remove(existing)
assert(tracker.pendingChangeCount() > 0, "moving and then deleting a saved object is not a cancelled creation")

resetPending()
local first = instance("Folder", "SameName")
local second = instance("Folder", "SameName")
add(first, storage)
add(second, storage)
remove(first)
pending(1, "duplicate-name identity preserves the surviving object")
remove(second)
pending(0, "both duplicate additions cancelled")

resetPending()
corner.TopRightRadius = roblox.UDim.new(0, 19)
corner.Changed:Fire("TopRightRadius")
temporary = instance("Folder", "Temporary")
add(temporary, storage)
remove(temporary)
pending(1, "unrelated existing property edit survives cancellation")

resetPending()
add(existing, storage)
saved = tracker.getState({})
tracker.getState({ ackSeq = saved.seq, runtimeId = "test" })
remove(existing)
add(existing, workspace)
local moved = tracker.getState({})
assert(table.find(moved.dirtyServices, "ReplicatedStorage") and table.find(moved.dirtyServices, "Workspace"), "both sides of cross-service move must remain pending")
remove(existing)
local removedMove = tracker.getState({})
assert(#removedMove.dirtyServices == 1 and removedMove.dirtyServices[1] == "ReplicatedStorage", "cancel only the unpublished destination, not the saved source deletion")
print("Pending changes: 15,200 transient instances, duplicate identities, moves and delayed acknowledgments passed")
local beforeGeometry = tracker.serviceGeneration("ReplicatedStorage")
deferred = {}
tracker.beginSuppress()
tracker.expectPropertyEvent(mesh, "CollisionFidelity", "Hull")
mesh.Changed:Fire("CollisionFidelity") -- Async setter reports its unchanged old value first.
mesh.CollisionFidelity = "Hull"
mesh.Changed:Fire("CollisionFidelity")
flushDeferred()
assert(tracker.serviceGeneration("ReplicatedStorage") == beforeGeometry, "unchanged async pre-write event is not a concurrent edit")
mesh.CollisionFidelity = "Box" -- A genuine change whose notification has not arrived yet.
tracker.expectPropertyEvent(mesh, "CollisionFidelity", "Hull")
mesh.Changed:Fire("CollisionFidelity")
flushDeferred()
assert(tracker.serviceGeneration("ReplicatedStorage") > beforeGeometry, "expectation must not overwrite an established fingerprint")
mesh.CollisionFidelity = "Hull"
mesh.Changed:Fire("CollisionFidelity")
flushDeferred()
local beforeBox = tracker.serviceGeneration("ReplicatedStorage")
mesh.Changed:Fire("CollisionFidelity") -- Box only notifies before changing its value.
mesh.CollisionFidelity = "Box"
assert(tracker.serviceGeneration("ReplicatedStorage") == beforeBox)
flushDeferred()
assert(tracker.serviceGeneration("ReplicatedStorage") > beforeBox, "pre-setter notification must capture the final Box value")
tracker.endSuppress(0)
tracker.stop()
deferred = nil
local restoredConfig = table.clone(trackerConfig)
restoredConfig.loadPendingStudioChanges = function()
	return { services = { "ReplicatedStorage" }, epoch = "prior", runtimeId = "previous" }
end
tracker = trackerModule.create(restoredConfig, { ReplicatedStorage = true })
tracker.getState({})
temporary = instance("Folder", "Temporary")
add(temporary, storage)
remove(temporary)
pending(1, "cancellation must not discard unknown changes restored after reconnect")
tracker.stop()
print("Corner property tracking: 200 coupled writes, legacy writes, cancellation and genuine concurrent edits passed")

-- A real DataModel.ItemChanged reports property names even for ValueBase.
-- Keep the fallback workload above, then verify shared observation separately.
testGame.ItemChanged = signal()
tracker = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
tracker.configurePropertyCandidates({ Part = { "Transparency" }, StringValue = { "Value" }, MeshPart = { "CollisionFidelity" } })
tracker.getState({})
assert(testGame.ItemChanged:Count() == 1)
assert(moving.Changed:Count() == 0 and moving.AttributeChanged:Count() == 1)
assert(value:GetPropertyChangedSignal("Parent"):Count() == 0)
tracker.beginChangeJournal("shared", { "ReplicatedStorage" })
for index = 1, 200 do
	value.Value = tostring(index)
	testGame.ItemChanged:Fire(value, "Value")
	local records = tracker.drainChangeJournal("shared")
	assert(#records == 1 and records[1].properties.Value.value == tostring(index))
end
local generationBeforeExpected = tracker.serviceGeneration("ReplicatedStorage")
tracker.beginSuppress()
tracker.expectPropertyEvent(value, "Value", "expected")
value.Value = "expected"
testGame.ItemChanged:Fire(value, "Value")
assert(tracker.serviceGeneration("ReplicatedStorage") == generationBeforeExpected)
tracker.endSuppress(0)
value.Name = "RenamedValue"
testGame.ItemChanged:Fire(value, "Name")
assert(tracker.serviceGeneration("ReplicatedStorage") > generationBeforeExpected)
moving.GetAttribute = function() return 17 end
moving.GetAttributes = function() return { Probe = 17 } end
moving.AttributeChanged:Fire("Probe")
local records = tracker.drainChangeJournal("shared")
local attributeFound = false
for _, record in records do
	if record.instance == moving and record.attributes.Probe then
		attributeFound = record.attributes.Probe.value == 17
	end
end
assert(attributeFound, "shared properties must retain separate attribute observation")
tracker.stop()
assert(testGame.ItemChanged:Count() == 0 and moving.AttributeChanged:Count() == 0)
print("Shared property observation: ValueBase changes, rename, expectations, attributes and teardown passed")
