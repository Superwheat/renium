local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")
local serde = require("@lune/serde")

local rbxDomSource = fs.readFile("tools/plugin_ws_bridge/rbx_dom_lua/init.module.lua")
local aliasStart = assert(rbxDomSource:find("local contentPropertyAliasesByClass =", 1, true))
local aliasEnd = assert(rbxDomSource:find("local function getPropertyNames(", aliasStart, true))
local contentAliases = luau.load(rbxDomSource:sub(aliasStart, aliasEnd - 1) .. "\nreturn getContentPropertyAliases", {
	environment = { database = serde.decode("json", fs.readFile("tools/plugin_ws_bridge/rbx_dom_lua/database.json")) },
})()
assert(contentAliases("Beam").texture == "TextureContent")
assert(contentAliases("MeshPart").meshid == "MeshContent")
assert(contentAliases("ImageLabel").image == "ImageContent")
assert(contentAliases("Beam") == contentAliases("Beam"), "content metadata must be cached by class")
assert(contentAliases("Part").texture == nil and next(contentAliases("UnknownClass")) == nil)
local contentTypes = { None = "None", Uri = "Uri", Object = "Object" }
local function content(uri)
	return { __type = "Content", SourceType = if uri == "" then "None" else "Uri", Uri = uri }
end

local function signal()
	local callbacks = {}
	return {
		Count = function() local count = 0 for _ in callbacks do count += 1 end return count end,
		Connect = function(_, callback)
			callbacks[callback] = true
			return { Disconnect = function() callbacks[callback] = nil end }
		end,
		Fire = function(_, ...)
			for callback in table.clone(callbacks) do callback(...) end
		end,
		Wait = function(self)
			local thread = coroutine.running()
			local connection
			connection = self:Connect(function(...)
				connection:Disconnect()
				assert(coroutine.resume(thread, ...))
			end)
			return coroutine.yield()
		end,
	}
end

local function instance(className, name, parent)
	local item = { ClassName = className, Name = name, Parent = parent, Archivable = true, children = {} }
	if parent then table.insert(parent.children, item) end
	for _, name in { "Changed", "AncestryChanged", "AttributeChanged", "DescendantAdded", "DescendantRemoving" } do
		item[name] = signal()
	end
	function item:IsA(class) return self.ClassName == class or class == "Instance"
		or class == "BaseScript" and (self.ClassName == "Script" or self.ClassName == "LocalScript")
		or class == "BasePart" and (self.ClassName == "Part" or self.ClassName == "MeshPart")
		or class == "ValueBase" and self.ClassName == "StringValue" end
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
local camera = instance("Camera", "Camera", storage)
local beam = instance("Beam", "Beam", storage)
local gui = instance("ScreenGui", "Gui", storage)
gui.ScreenInsets = roblox.Enum.ScreenInsets.CoreUISafeInsets
local localScript = instance("LocalScript", "LocalScript", storage)
localScript.Disabled = false
local terrain = setmetatable(instance("Terrain", "Terrain", storage), { __index = function(_, key)
	if key == "Decoration" then error("Decoration is not a readable member") end
	return nil
end })
beam.TextureContent = content("")
mesh.MeshContent = content("")
camera.FieldOfView = 70
camera.MaxAxisFieldOfView = 70
camera.DiagonalFieldOfView = 80
mesh.CollisionFidelity = "Box"
mesh.Color = roblox.Color3.new(1, 1, 1)
mesh.BrickColor = roblox.BrickColor.new(mesh.Color)
local names = { "TopLeftRadius", "TopRightRadius", "BottomLeftRadius", "BottomRightRadius" }
for _, name in names do corner[name] = roblox.UDim.new(0, 8) end
local services = {
	ReplicatedStorage = storage, Workspace = workspace, EncodingService = {},
	CollectionService = { GetTags = function() return {} end, GetAllTags = function() return {} end,
		GetTagged = function() return {} end, GetInstanceAddedSignal = function() return signal() end,
		GetInstanceRemovedSignal = function() return signal() end,
		TagAdded = signal(), TagRemoved = signal() },
	CoreGui = instance("CoreGui", "CoreGui", testGame),
	HttpService = { GenerateGUID = function() return "owned-relay-test" end },
	RunService = { Heartbeat = signal() },
}
function testGame:GetService(name) return assert(services[name], name) end
local codec = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeValueCodec.module.lua"), { environment = { game = testGame } })()
local dependencies = { BridgeScriptDocuments = { readSource = function(instance) return true, instance.Source end }, BridgeValueCodec = codec, BridgeValueEquality = require("../BridgeValueEquality.module"),
	BridgeIdentity = require("../BridgeIdentity.module"),
	BridgeContent = require("../BridgeContent.module"),
	RbxDom = { getContentPropertyAliases = contentAliases, findCanonicalPropertyDescriptor = function() return { dataType = "UDim" } end } }
local deferred = nil
local capturedDelays = nil
local function flushDeferred()
	while #deferred > 0 do
		local callbacks = deferred
		deferred = {}
		for _, callback in callbacks do callback() end
	end
end
local createdRelays = {}
local trackerModule = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeStudioChanges.module.lua"), {
	environment = {
		game = testGame,
		typeof = function(value) return if type(value) == "table" and value.__type then value.__type
			else if type(value) == "table" and value.ClassName then "Instance" else typeof(value) end,
		Enum = { ContentSourceType = contentTypes, ScreenInsets = roblox.Enum.ScreenInsets },
		script = { Parent = dependencies }, require = function(module) return module end,
		Instance = { new = function(class)
			if class == "ObjectValue" then
				local relay = instance(class, "Notify")
				function relay:Destroy() self.destroyed = true self.Parent = nil end
				function relay:SetAttribute(name, value) self[name] = value end
				table.insert(createdRelays, relay)
				return relay
			end
			assert(class == "BindableEvent")
			local event = signal()
			return { Event = event, Fire = function(_, ...) event:Fire(...) end, Destroy = function() end }
		end },
		task = { delay = function(seconds, callback)
			if capturedDelays then table.insert(capturedDelays, { seconds = seconds, callback = callback }) end
		end, spawn = function() end, defer = function(callback)
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
tracker.configurePropertyCandidates({ UICorner = candidates, Part = { "Transparency" }, StringValue = { "Value" }, MeshPart = { "MeshContent", "CollisionFidelity", "Color", "BrickColor" }, Beam = { "TextureContent" }, Camera = { "FieldOfView", "MaxAxisFieldOfView", "DiagonalFieldOfView" } })
tracker.getState({ services = { "ReplicatedStorage" } })
assert(moving.Changed:Count() == 1 and moving.AttributeChanged:Count() == 1 and moving.AncestryChanged:Count() == 0)
assert(value:GetPropertyChangedSignal("Parent"):Count() == 1)
tracker.beginSuppress()
tracker.beginChangeJournal("corner", { "ReplicatedStorage" })
local generation = tracker.serviceGeneration("ReplicatedStorage")
value.Changed:Fire(value.Value)
assert(tracker.serviceGeneration("ReplicatedStorage") == generation,
	"A generic Changed notification without a Value property event invented an edit")
assert(#tracker.drainChangeJournal("corner") == 0)
for _ = 1, 100 do tracker.samplePropertyChange(terrain, "Decoration") end
assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "failed synthetic reads must not invent edits")
assert(#tracker.drainChangeJournal("corner") == 0)
tracker.expectPropertyEvent(terrain, "Decoration", true)
terrain.Changed:Fire("Decoration")
generation += 1
assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "actual unreadable-property events must still conflict")
local unreadableJournal = tracker.drainChangeJournal("corner")
assert(#unreadableJournal == 1 and unreadableJournal[1].properties.Decoration.captured == false)
for index = 1, 100 do
	local disabled = index % 2 == 1
	tracker.expectPropertyEvent(localScript, if index % 3 == 0 then "Enabled" else "Disabled",
		if index % 3 == 0 then not disabled else disabled)
	localScript.Disabled = disabled
	for _, name in (if index % 2 == 0 then { "Enabled", "Disabled" } else { "Disabled", "Enabled" }) do localScript.Changed:Fire(name) end
	assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "Enabled/Disabled aliases became outside edits")
end
local scriptToken = tracker.expectPropertyEvent(localScript, "Enabled", false)
tracker.cancelExpectedEvent(scriptToken)
localScript.Disabled = true
localScript.Changed:Fire("Enabled")
localScript.Changed:Fire("Disabled")
generation += 1
assert(tracker.serviceGeneration("ReplicatedStorage") == generation)
local scriptJournal = tracker.drainChangeJournal("corner")
assert(#scriptJournal == 1 and scriptJournal[1].properties.Disabled.value == true and scriptJournal[1].properties.Enabled == nil)
trackerConfig.LUA_SOURCE_CLASS.LocalScript = true
localScript.Source = "old"
tracker.expectPropertyEvent(localScript, "Source", "return 41\r\n")
localScript.Source = "return 41\n"
localScript.Changed:Fire("Source")
assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "Source newline normalization became an outside edit")
localScript.Source = "return 42\n"
localScript.Changed:Fire("Source")
generation += 1
assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "A real Source edit was hidden by newline normalization")
local sourceJournal = tracker.drainChangeJournal("corner")
assert(#sourceJournal == 1 and sourceJournal[1].properties.Source.value == "return 42\n")
trackerConfig.LUA_SOURCE_CLASS.LocalScript = nil
for index = 1, 50 do
	for _, inset in roblox.Enum.ScreenInsets:GetEnumItems() do
		tracker.expectPropertyEvent(gui, "ScreenInsets", inset)
		gui.ScreenInsets = inset
		for _, name in (if index % 2 == 0 then { "IgnoreGuiInset", "ScreenInsets" } else { "ScreenInsets", "IgnoreGuiInset" }) do gui.Changed:Fire(name) end
		assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "own inset alias became an outside edit")
	end
end
for _, initial in roblox.Enum.ScreenInsets:GetEnumItems() do
	gui.ScreenInsets = initial
	gui.Changed:Fire("ScreenInsets")
	generation = tracker.serviceGeneration("ReplicatedStorage")
	tracker.drainChangeJournal("corner")
	for _, value in { true, false, true } do
		local token = tracker.expectPropertyEvent(gui, "IgnoreGuiInset", value)
		gui.ScreenInsets = if not value then roblox.Enum.ScreenInsets.CoreUISafeInsets
			elseif gui.ScreenInsets == roblox.Enum.ScreenInsets.CoreUISafeInsets then roblox.Enum.ScreenInsets.DeviceSafeInsets else gui.ScreenInsets
		gui.Changed:Fire("IgnoreGuiInset")
		gui.Changed:Fire("ScreenInsets")
		tracker.cancelExpectedEvent(token)
		assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "legacy inset expectation lost the safe-area mode")
	end
end
tracker.drainChangeJournal("corner")
local insetToken = tracker.expectPropertyEvent(gui, "ScreenInsets", roblox.Enum.ScreenInsets.None)
tracker.cancelExpectedEvent(insetToken)
gui.ScreenInsets = roblox.Enum.ScreenInsets.None
gui.Changed:Fire("IgnoreGuiInset")
gui.Changed:Fire("ScreenInsets")
generation += 1
assert(tracker.serviceGeneration("ReplicatedStorage") == generation, `expected {generation}, got {tracker.serviceGeneration("ReplicatedStorage")}`)
local insetJournal = tracker.drainChangeJournal("corner")
assert(#insetJournal == 1 and insetJournal[1].properties.ScreenInsets.value == gui.ScreenInsets
	and insetJournal[1].properties.IgnoreGuiInset == nil, "outside insets must replay the exact enum, not its boolean alias")
for _, test in { { beam, "Texture", "TextureContent" }, { mesh, "MeshId", "MeshContent" } } do
	local target, alias, canonical = table.unpack(test)
	for index = 1, 100 do
		local uri = if index % 3 == 0 then "" else `rbxassetid://{index}`
		tracker.expectPropertyEvent(target, if index % 2 == 0 then alias else canonical, uri)
		target[canonical] = content(uri)
		for _, name in (if index % 2 == 0 then { alias, canonical } else { canonical, alias }) do target.Changed:Fire(name) end
		assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "own Content URI/None or legacy alias became an outside edit")
	end
	assert(#tracker.drainChangeJournal("corner") == 0)
	local cancelled = tracker.expectPropertyEvent(target, canonical, "rbxassetid://outside")
	tracker.cancelExpectedEvent(cancelled)
	target[canonical] = content("rbxassetid://outside")
	target.Changed:Fire(alias)
	target.Changed:Fire(canonical)
	generation += 1
	assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "outside Content edit must conflict exactly once")
	local journal = tracker.drainChangeJournal("corner")
	assert(#journal == 1 and journal[1].properties[canonical].value == target[canonical])
	assert(journal[1].properties[alias] == nil, "rollback must not overwrite Object Content through a URI alias")
	local object = instance("Folder", "ContentObject")
	local objectContent = { __type = "Content", SourceType = "Object", Object = object }
	tracker.expectPropertyEvent(target, canonical, objectContent)
	target[canonical] = objectContent
	target.Changed:Fire(canonical)
	assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "Object Content must retain its reference")
	target[canonical] = content("")
	target.Changed:Fire(alias)
	generation += 1
	assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "Object Content must not compare equal to an empty URI")
	tracker.drainChangeJournal("corner")
end
print("Content aliases: 200 own URI/None writes, both signal orders, cancellation and Object Content preserved")
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
for index = 1, 200 do
	local color = roblox.Color3.new(index / 200, 0.125, 0.75)
	tracker.expectPropertyEvent(mesh, "Color", color)
	mesh.Color = color
	mesh.BrickColor = roblox.BrickColor.new(color)
	for _, name in (if index % 2 == 0 then { "Color", "BrickColor" } else { "BrickColor", "Color" }) do mesh.Changed:Fire(name) end
	assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "Color's BrickColor alias became an outside edit")
end
local brick = roblox.BrickColor.new("Bright red")
tracker.expectPropertyEvent(mesh, "BrickColor", brick)
mesh.BrickColor = brick
mesh.Color = brick.Color
mesh.Changed:Fire("Color")
mesh.Changed:Fire("BrickColor")
assert(tracker.serviceGeneration("ReplicatedStorage") == generation)
assert(#tracker.drainChangeJournal("corner") == 0)
local colorToken = tracker.expectPropertyEvent(mesh, "Color", roblox.Color3.new(0, 0, 0))
tracker.cancelExpectedEvent(colorToken)
mesh.Color = roblox.Color3.new(0.234, 0.456, 0.678)
mesh.BrickColor = roblox.BrickColor.new(mesh.Color)
mesh.Changed:Fire("BrickColor")
mesh.Changed:Fire("Color")
assert(tracker.serviceGeneration("ReplicatedStorage") == generation + 1)
local colorJournal = tracker.drainChangeJournal("corner")
assert(#colorJournal == 1 and colorJournal[1].properties.Color.value == mesh.Color)
assert(colorJournal[1].properties.BrickColor == nil, "Replay must preserve exact Color, not the quantized BrickColor")
generation += 1
for _, requested in { "FieldOfView", "MaxAxisFieldOfView", "DiagonalFieldOfView" } do
	tracker.expectPropertyEvent(camera, requested, 85)
	camera.FieldOfView += 1
	camera[requested] = 84.99999237060547
	for _, signalName in { "DiagonalFieldOfView", "FieldOfView", "MaxAxisFieldOfView" } do
		camera.Changed:Fire(signalName)
	end
	assert(tracker.serviceGeneration("ReplicatedStorage") == generation, "rounded FOV and aliases became outside edits")
end
local fovToken = tracker.expectPropertyEvent(camera, "FieldOfView", 85)
tracker.cancelExpectedEvent(fovToken)
camera.FieldOfView = 60
for _, signalName in { "DiagonalFieldOfView", "FieldOfView", "MaxAxisFieldOfView" } do camera.Changed:Fire(signalName) end
assert(tracker.serviceGeneration("ReplicatedStorage") == generation + 1, "outside FOV change must still conflict exactly once")
local fovJournal = tracker.drainChangeJournal("corner")
assert(#fovJournal == 1 and fovJournal[1].properties.FieldOfView.value == 60)
generation += 1
local token = tracker.expectPropertyEvent(corner, "CornerRadius", external)
tracker.cancelExpectedEvent(token)
change("TopLeftRadius", external, true)
assert(tracker.serviceGeneration("ReplicatedStorage") == generation + 1, "real edit must still conflict")
local journal = tracker.drainChangeJournal("corner")
assert(#journal == 1 and journal[1].properties.TopLeftRadius.value == external)
assert(journal[1].properties.CornerRadius == nil, "journal must not replay the legacy all-corners setter")
local function changeAndReadOrdinal(target, index)
	if target:IsA("ValueBase") then
		target.Value = tostring(index)
		target.Changed:Fire(target.Value)
		target:GetPropertyChangedSignal("Value"):Fire()
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
local function add(target, parent, childFirst)
	target.Parent = parent
	table.insert(parent.children, target)
	local service = if target:IsDescendantOf(storage) then storage else workspace
	local descendants = target:GetDescendants()
	if childFirst then
		for index = #descendants, 1, -1 do service.DescendantAdded:Fire(descendants[index]) end
		service.DescendantAdded:Fire(target)
	else
		service.DescendantAdded:Fire(target)
		for _, descendant in descendants do service.DescendantAdded:Fire(descendant) end
	end
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
local acknowledged = tracker.getState({ ackSeq = saved.seq, runtimeId = "test" })
assert(acknowledged.seq == saved.seq and acknowledged.snapshotSeq > saved.snapshotSeq,
	"acknowledgments must order pending-state snapshots without inventing mutations")
assert(acknowledged.changeCount == 0 and saved.changeCount > 0)
local polled = tracker.getState({ compact = true })
assert(polled.seq == acknowledged.seq and polled.snapshotSeq > acknowledged.snapshotSeq,
	"compact polling must preserve snapshot order")
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
for _, requested in { "Hull", "Box", "Hull", "Box" } do
	tracker.expectPropertyEvent(mesh, "CollisionFidelity", requested)
	mesh.Changed:Fire("CollisionFidelity")
	flushDeferred() -- Cooking is still pending when the deferred signal runs.
	mesh.CollisionFidelity = requested -- No final engine signal.
	tracker.samplePropertyChange(mesh, "CollisionFidelity")
	assert(tracker.serviceGeneration("ReplicatedStorage") == beforeGeometry,
		"verified cooking completion must not leave a stale fingerprint for the next transaction")
end
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
-- A manual native write can finish after both engine samples saw its old value.
resetPending()
mesh.Changed:Fire("CollisionFidelity")
flushDeferred()
mesh.CollisionFidelity = "Hull"
tracker.samplePropertyChange(mesh, "CollisionFidelity")
local manualGeometry = tracker.getState({})
assert(manualGeometry.changeCount > 0, "completed manual native edits must remain pending")
tracker.beginSuppress()
tracker.expectPropertyEvent(mesh, "CollisionFidelity", "Box")
mesh.Changed:Fire("CollisionFidelity")
flushDeferred()
mesh.CollisionFidelity = "Box"
tracker.samplePropertyChange(mesh, "CollisionFidelity")
assert(tracker.serviceGeneration("ReplicatedStorage") == manualGeometry.seq,
	"a completed earlier edit must not become a false conflict in the next native transaction")
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

for _, shared in { false, true } do
	testGame.ItemChanged = if shared then signal() else nil
	for index = 1, 30 do
		tracker = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
		local exportEvents = 0
		local withExports = index % 2 == 0
		if withExports then
			tracker.observeExports(function(serviceName)
				assert(serviceName == "ReplicatedStorage")
				exportEvents += 1
			end, function()
				exportEvents += 1
			end)
		end
		tracker.getState({})
		assert(storage.DescendantAdded:Count() == 1 and storage.DescendantRemoving:Count() == 1)
		if shared then assert(testGame.ItemChanged:Count() == 1) end
		assert(moving.AttributeChanged:Count() == 1)
		assert(moving.Changed:Count() == (if shared then 0 else 1))
		local generation = tracker.serviceGeneration("ReplicatedStorage")
		moving.GetAttribute = function() return index end
		moving.AttributeChanged:Fire("Probe")
		assert(tracker.serviceGeneration("ReplicatedStorage") > generation)
		tracker.getState({ stop = true })
		assert(moving.AttributeChanged:Count() == 0 and moving.Changed:Count() == 0)
		local before = exportEvents
		storage.DescendantAdded:Fire(moving)
		storage.DescendantRemoving:Fire(moving)
		if shared then testGame.ItemChanged:Fire(moving, "Name") end
		assert(exportEvents == before + (if withExports then (if shared then 3 else 2) else 0))
		tracker.getState({})
		assert(moving.AttributeChanged:Count() == 1 and storage.DescendantAdded:Count() == 1)
		tracker.stop()
		assert(moving.AttributeChanged:Count() == 0 and moving.Changed:Count() == 0)
		assert(value:GetPropertyChangedSignal("Parent"):Count() == 0)
		assert(storage.DescendantAdded:Count() == 0 and storage.DescendantRemoving:Count() == 0)
		if shared then assert(testGame.ItemChanged:Count() == 0) end
	end
end
print("Tracker restart: attributes and listener cleanup passed in both observation modes")

-- Initial indexing must retain source-bearing ancestors and use the same
-- export filter for both the instance list and the non-Archivable count.
local codeRoot = instance("Folder", "CodeRoot", storage)
local codeBranch = instance("Folder", "CodeBranch", codeRoot)
local nestedScript = instance("ModuleScript", "Nested", codeBranch)
local emptyBranch = instance("Folder", "EmptyBranch", codeRoot)
local indexedConfig = table.clone(trackerConfig)
indexedConfig.LUA_SOURCE_CLASS = { ModuleScript = true }
indexedConfig.includeExportInstance = function(_, target) return target ~= sibling end
sibling.Archivable = false
moving.Archivable = false
tracker = trackerModule.create(indexedConfig, { ReplicatedStorage = true })
tracker.setOptions({ onlyCodeMode = true })
tracker.getState({})
assert(codeRoot.AttributeChanged:Count() == 1 and codeBranch.AttributeChanged:Count() == 1)
assert(nestedScript.AttributeChanged:Count() == 1 and emptyBranch.AttributeChanged:Count() == 0)
assert(moving.AttributeChanged:Count() == 0 and tracker.hasNonArchivable("ReplicatedStorage") == nil)
tracker.setOptions({ onlyCodeMode = false })
assert(moving.AttributeChanged:Count() == 1 and sibling.AttributeChanged:Count() == 0)
assert(tracker.hasNonArchivable("ReplicatedStorage") == true)
local exported = tracker.exportInstances("ReplicatedStorage")
assert(exported[1] == storage and table.find(exported, moving) and not table.find(exported, sibling))
moving.Archivable = true
testGame.ItemChanged:Fire(moving, "Archivable")
assert(tracker.hasNonArchivable("ReplicatedStorage") == false, "excluded instances must not inflate the count")
tracker.invalidateExportCache("ReplicatedStorage")
assert(tracker.hasNonArchivable("ReplicatedStorage") == false)
assert(not table.find(tracker.exportInstances("ReplicatedStorage"), sibling))
do
	local getDescendants = storage.GetDescendants
	local scans = 0
	storage.GetDescendants = function(self)
		scans += 1
		return getDescendants(self)
	end
	-- Model the interval before deferred add/remove signals are delivered.
	local first = instance("Folder", "DeferredFirst", storage)
	local second = instance("Folder", "DeferredSecond", storage)
	first.Archivable = false
	second.Archivable = false
	tracker.invalidateExportCache("ReplicatedStorage")
	tracker.invalidateExportCache("ReplicatedStorage")
	assert(scans == 0, "invalidation must not rebuild the tree")
	assert(tracker.hasNonArchivable("ReplicatedStorage") == true)
	assert(scans == 1 and table.find(tracker.exportInstances("ReplicatedStorage"), first))
	assert(scans == 1, "count and instance list must share one refresh")
	storage.DescendantAdded:Fire(first)
	storage.DescendantAdded:Fire(second)
	assert(tracker.hasNonArchivable("ReplicatedStorage") == true)
	assert(scans == 2)
	for index, target in { first, second } do
		table.remove(storage.children, assert(table.find(storage.children, target)))
		target.Parent = nil
		tracker.invalidateExportCache("ReplicatedStorage")
		assert(tracker.hasNonArchivable("ReplicatedStorage") == (index == 1))
		assert(not table.find(tracker.exportInstances("ReplicatedStorage"), target))
		storage.DescendantRemoving:Fire(target)
		assert(tracker.hasNonArchivable("ReplicatedStorage") == (index == 1),
			"late removal must not erase another non-Archivable instance")
	end
	assert(scans == 6)
	storage.GetDescendants = getDescendants
end
tracker.setOptions({ onlyCodeMode = true })
deferred = {}
storage.DescendantRemoving:Fire(nestedScript)
table.remove(codeBranch.children, assert(table.find(codeBranch.children, nestedScript)))
nestedScript.Parent = nil
flushDeferred()
assert(codeRoot.AttributeChanged:Count() == 0 and codeBranch.AttributeChanged:Count() == 0)
tracker.stop()
deferred = nil
print("Tracker indexing: nested source counts, mode transitions, export filtering and Archivable changes passed")

-- Adding one subtree does not change the internal sibling order 2400 times.
tracker = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
tracker.configurePropertyCandidates({ Part = { "Transparency", "CFrame", "Color", "Label", "Unset", "Flag" } })
tracker.getState({ reset = true })
do
	local wrapper = instance("Folder", "DetachedWrapper")
	local root = instance("Folder", "DetachedRoot", wrapper)
	for _ = 1, 100 do instance("Part", "Child", root) end
	local getDescendants = root.GetDescendants
	root.GetDescendants = function() error("detached parent expectation must not scan its subtree") end
	tracker.beginSuppress()
	local token = tracker.expectParentChange(root, nil)
	assert(#token.tokens == 1, "retain only the root Parent expectation while detached")
	root.GetDescendants = getDescendants
	root.Parent = nil
	local generation = tracker.serviceGeneration("ReplicatedStorage")
	add(root, storage)
	assert(tracker.serviceGeneration("ReplicatedStorage") > generation, "detached preparation must not swallow a live addition")
	generation = tracker.serviceGeneration("ReplicatedStorage")
	remove(root)
	assert(tracker.serviceGeneration("ReplicatedStorage") > generation, "detached preparation must not swallow a later live removal")
	tracker.endSuppress(0)
	tracker.getState({ reset = true })
end
local deepRoot = instance("Folder", "DeepRoot")
local deepParent = deepRoot
local ancestorVisits = 0
for index = 1, 24 do
	deepParent = instance("Folder", `Depth{index}`, deepParent)
	local isDescendant = deepParent.IsDescendantOf
	deepParent.IsDescendantOf = function(self, ancestor)
		ancestorVisits += 1
		return isDescendant(self, ancestor)
	end
end
tracker.beginSuppress()
tracker.expectParentChange(deepRoot, storage)
add(deepRoot, storage, true)
tracker.endSuppress(0)
ancestorVisits = 0
local wide = instance("Folder", "Wide")
for _ = 1, 2400 do
	local child = instance("Part", "Duplicate", wide)
	child.Transparency = 0
	child.CFrame = roblox.CFrame.new(1, 2, 3)
	child.Color = roblox.Color3.new(0.1, 0.2, 0.3)
	child.Label = string.rep("unchanged", 32)
	child.Flag = false
end
local siblingScans = 0
function wide:GetChildren()
	siblingScans += 1
	return self.children
end
tracker.beginSuppress()
local attachmentProfile = {}
tracker.expectParentChange(wide, deepParent, attachmentProfile)
trackerConfig.syncProfile = { attachment = attachmentProfile }
add(wide, deepParent, true)
trackerConfig.syncProfile.attachment = nil
tracker.endSuppress(0)
assert(attachmentProfile.connectedInstances == 2401 and attachmentProfile.primedInstances == 2401,
	"descendant-first attachment profiling must include ancestors connected by a child event")
assert(attachmentProfile.baselineMs >= 0 and attachmentProfile.attributeBaselineMs >= 0 and attachmentProfile.listenerSetupMs >= 0)
assert(attachmentProfile.trackerAddedCallbacks == 2401)
assert(attachmentProfile.trackerAddedCallbacksMs >= attachmentProfile.trackerPreExpectationMs
	and attachmentProfile.trackerPreExpectationMs >= 0)
assert(siblingScans <= 2, `bulk attachment rescanned siblings {siblingScans} times`)
assert(ancestorVisits < 30, `bulk attachment revisited connected ancestors {ancestorVisits} times`)
assert(tracker.pendingChangeCount() == 0)
tracker.beginChangeJournal("wide", { "ReplicatedStorage" })
local last = wide.children[#wide.children]
for _, property in { "Transparency", "CFrame", "Color", "Label", "Unset", "Flag" } do
	testGame.ItemChanged:Fire(last, property)
end
assert(#tracker.drainChangeJournal("wide") == 0, "lazy baselines must suppress unchanged first events")
last.Unset = "nil"
last.Flag = true
testGame.ItemChanged:Fire(last, "Unset")
testGame.ItemChanged:Fire(last, "Flag")
local primitiveJournal = tracker.drainChangeJournal("wide")
assert(#primitiveJournal == 1 and primitiveJournal[1].properties.Unset.value == "nil"
	and primitiveJournal[1].properties.Flag.value == true, "raw nil/false baselines must not hide first edits")
last.CFrame = roblox.CFrame.new(4, 5, 6)
testGame.ItemChanged:Fire(last, "CFrame")
assert(#tracker.drainChangeJournal("wide") == 1, "lazy baseline must retain a changed first event")
last.Transparency = 0.25
testGame.ItemChanged:Fire(last, "Transparency")
local journal = tracker.drainChangeJournal("wide")
assert(#journal == 1 and journal[1].pathOrdinals[#journal[1].pathOrdinals] == 2400)
local removed = wide.children[1]
deferred = {}
remove(removed)
testGame.ItemChanged:Fire(removed, "Parent")
flushDeferred()
deferred = nil
tracker.drainChangeJournal("wide")
last.Transparency = 0.5
testGame.ItemChanged:Fire(last, "Transparency")
journal = tracker.drainChangeJournal("wide")
assert(#journal == 1 and journal[1].pathOrdinals[#journal[1].pathOrdinals] == 2399)
tracker.stop()
remove(wide)
remove(deepRoot)
print("Bulk subtree tracking: bounded ancestor/sibling scans, exact duplicate ordinals and subsequent deletion preserved")

-- Parent observations compare object identity; names/ordinals have their own events.
tracker = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
tracker.getState({ reset = true })
local parentRoot = instance("Folder", "Parents")
local parentA = instance("Folder", "Duplicate", parentRoot)
local parentB = instance("Folder", "Duplicate", parentRoot)
local parentChild = instance("Part", "Child", parentA)
tracker.beginSuppress()
tracker.expectParentChange(parentRoot, storage)
add(parentRoot, storage, true)
tracker.endSuppress(0)
tracker.beginChangeJournal("parents", { "ReplicatedStorage" })
for cycle = 1, 100 do
	parentRoot.Name = `Parents{cycle}`
	testGame.ItemChanged:Fire(parentRoot, "Name")
	tracker.drainChangeJournal("parents")
	testGame.ItemChanged:Fire(parentChild, "Parent")
	assert(#tracker.drainChangeJournal("parents") == 0, "ancestor rename invented a Parent change")
	table.remove(parentA.children, assert(table.find(parentA.children, parentChild)))
	table.insert(parentB.children, parentChild)
	parentChild.Parent = parentB
	testGame.ItemChanged:Fire(parentChild, "Parent")
	local records = tracker.drainChangeJournal("parents")
	assert(#records == 1 and records[1].properties.Parent.value == parentB, "same-named parent move was lost")
	tracker.beginSuppress()
	tracker.expectParentChange(parentChild, parentA)
	table.remove(parentB.children, assert(table.find(parentB.children, parentChild)))
	table.insert(parentA.children, parentChild)
	parentChild.Parent = parentA
	testGame.ItemChanged:Fire(parentChild, "Parent")
	assert(#tracker.drainChangeJournal("parents") == 0, "expected Parent move was reported as external")
	tracker.endSuppress(0)
end
tracker.beginSuppress()
tracker.expectPropertyEvent(parentChild, "parent", parentA)
testGame.ItemChanged:Fire(parentChild, "Parent")
for _, parent in { parentB, parentA } do
	table.remove(parentChild.Parent.children, assert(table.find(parentChild.Parent.children, parentChild)))
	table.insert(parent.children, parentChild)
	parentChild.Parent = parent
	testGame.ItemChanged:Fire(parentChild, "Parent")
	local records = tracker.drainChangeJournal("parents")
	assert(#records == 1 and records[1].properties.Parent.value == parent, "no-op expectation swallowed a later parent change")
end
tracker.endSuppress(0)
tracker.stop()
remove(parentRoot)
print("Parent baselines: 100 ancestor renames, same-named parent moves and exact expected moves passed")

tracker = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
tracker.configurePropertyCandidates({ ObjectValue = { "Value" }, Part = { "Name" } })
tracker.getState({ reset = true })
local referenceRoot = instance("Folder", "References")
local referenced = instance("Part", "Same", referenceRoot)
local otherReferenced = instance("Part", "Same", referenceRoot)
local holder = instance("ObjectValue", "Holder", referenceRoot)
holder.Value = referenced
tracker.beginSuppress()
tracker.expectParentChange(referenceRoot, storage)
add(referenceRoot, storage, true)
tracker.endSuppress(0)
tracker.beginChangeJournal("references", { "ReplicatedStorage" })
for cycle = 1, 100 do
	referenced.Name = `Renamed{cycle}`
	testGame.ItemChanged:Fire(referenced, "Name")
	assert(tracker.getState({ start = false }).referencePathsMayChange, "rename must still refresh exported references")
	tracker.drainChangeJournal("references")
	testGame.ItemChanged:Fire(holder, "Value")
	assert(#tracker.drainChangeJournal("references") == 0, "referent rename invented a Ref edit")
	holder.Value = otherReferenced
	testGame.ItemChanged:Fire(holder, "Value")
	local records = tracker.drainChangeJournal("references")
	assert(#records == 1 and records[1].properties.Value.value == otherReferenced, "new Ref identity was lost")
	tracker.beginSuppress()
	tracker.expectPropertyEvent(holder, "Value", referenced)
	holder.Value = referenced
	testGame.ItemChanged:Fire(holder, "Value")
	assert(#tracker.drainChangeJournal("references") == 0, "expected Ref assignment became an outside edit")
	tracker.endSuppress(0)
end
tracker.beginSuppress()
local samePathObject = table.clone(referenced)
holder.Value = samePathObject
testGame.ItemChanged:Fire(holder, "Value")
local replacementRecords = tracker.drainChangeJournal("references")
assert(#replacementRecords == 1 and replacementRecords[1].properties.Value.value == samePathObject,
	"distinct objects with identical names, parents and values must not compare equal")
tracker.expectPropertyEvent(holder, "Value", referenced)
holder.Value = referenced
testGame.ItemChanged:Fire(holder, "Value")
assert(#tracker.drainChangeJournal("references") == 0)
local referenceToken = tracker.expectPropertyEvent(holder, "Value", otherReferenced)
tracker.cancelExpectedEvent(referenceToken)
holder.Value = otherReferenced
testGame.ItemChanged:Fire(holder, "Value")
local referenceRecords = tracker.drainChangeJournal("references")
assert(#referenceRecords == 1 and referenceRecords[1].properties.Value.value == otherReferenced)
tracker.endSuppress(0)
tracker.stop()
remove(referenceRoot)
print("Reference baselines: 100 referent renames, identity changes, expected assignments and cancellation passed")

local previousDescriptor = dependencies.RbxDom.findCanonicalPropertyDescriptor
dependencies.RbxDom.findCanonicalPropertyDescriptor = function(class, name)
	if name == "Origin" or name == "Pivot Offset" then
		return { className = "PVInstance", scriptability = "None", serialization = "DoesNotSerialize" }
	elseif name == "SavedProtected" then
		return { className = "PVInstance", scriptability = "None", serialization = "Serializes" }
	elseif name == "OtherDisplay" then
		return { className = "OtherClass", scriptability = "None", serialization = "DoesNotSerialize" }
	end
	return previousDescriptor(class, name)
end
tracker = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
tracker.configurePropertyCandidates({ Part = { "Origin", "Pivot Offset", "PivotOffset", "SavedProtected", "OtherDisplay", "Unknown" },
	Model = { "Scale" } })
tracker.getState({ reset = true })
local pivotRoot = instance("Folder", "PivotBaseline")
local reads = {}
local pivotPart = setmetatable(instance("Part", "Part", pivotRoot), { __index = function(_, name)
	if name == "Origin" or name == "Pivot Offset" or name == "SavedProtected" or name == "OtherDisplay" or name == "Unknown" then
		reads[name] = (reads[name] or 0) + 1
		error("no direct getter")
	end
	return nil
end })
pivotPart.PivotOffset = roblox.CFrame.new()
local scaledModel = instance("Model", "Model", pivotRoot)
local scale = 1
function scaledModel:GetScale() return scale end
tracker.beginSuppress()
tracker.expectParentChange(pivotRoot, storage)
add(pivotRoot, storage, true)
tracker.endSuppress(0)
assert(reads.Origin == nil and reads["Pivot Offset"] == nil, "priming attempted inspector-only getters")
assert(reads.SavedProtected == 1 and reads.OtherDisplay == 1 and reads.Unknown == 1,
	"virtual-pivot optimization discarded saved, unrelated or unknown properties")
tracker.beginChangeJournal("pivot-baselines", { "ReplicatedStorage" })
testGame.ItemChanged:Fire(pivotPart, "PivotOffset")
testGame.ItemChanged:Fire(scaledModel, "Scale")
assert(#tracker.drainChangeJournal("pivot-baselines") == 0, "real pivot/scale baselines were omitted")
pivotPart.PivotOffset = roblox.CFrame.new(1, 2, 3)
scale = 2
testGame.ItemChanged:Fire(pivotPart, "PivotOffset")
testGame.ItemChanged:Fire(scaledModel, "Scale")
local pivotRecords = tracker.drainChangeJournal("pivot-baselines")
assert(#pivotRecords == 2 and pivotRecords[1].properties.PivotOffset.value == pivotPart.PivotOffset
	and pivotRecords[2].properties.Scale.value == 2)
tracker.stop()
remove(pivotRoot)
dependencies.RbxDom.findCanonicalPropertyDescriptor = previousDescriptor
print("Pivot baselines: virtual inspector fields skipped, saved/unknown fields attempted, real pivot and scale edits retained")

do
	tracker = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
	tracker.configurePropertyCandidates({ Part = {
		"CFrame", "Position", "Orientation", "Rotation", "Color", "BrickColor", "Color3uint8",
		"AssemblyLinearVelocity", "CurrentPhysicalProperties", "Flag", "Unset",
	}, Beam = { "Texture", "TextureContent" } })
	tracker.getState({ reset = true })
	local root = instance("Folder", "CanonicalBaselines")
	local parts, valuesByPart, readsByPart = {}, {}, {}
	for i = 1, 100 do
		local values = { CFrame = roblox.CFrame.new(i, 0, 0), Color = roblox.Color3.new(0.1, 0.2, 0.3), Flag = false }
		local readsForPart = {}
		local part = setmetatable(instance("Part", `Part{i}`, root), { __index = function(_, name)
			readsForPart[name] = (readsForPart[name] or 0) + 1
			return values[name]
		end })
		parts[i], valuesByPart[part], readsByPart[part] = part, values, readsForPart
	end
	local fallbackReads = {}
	local fallbackTexture = "rbxassetid://123"
	local fallbackBeam = setmetatable(instance("Beam", "LegacyTexture", root), { __index = function(_, name)
		fallbackReads[name] = (fallbackReads[name] or 0) + 1
		if name == "TextureContent" then error("canonical getter unavailable") end
		return if name == "Texture" then fallbackTexture else nil
	end })
	tracker.beginSuppress()
	tracker.expectParentChange(root, storage)
	add(root, storage, true)
	tracker.endSuppress(0)
	tracker.beginChangeJournal("canonical-baselines", { "ReplicatedStorage" })
	for _, part in parts do
		local propertyReads = readsByPart[part]
		assert(propertyReads.CFrame == 1 and propertyReads.Color == 1, "canonical baseline read more than once")
		assert(propertyReads.Position == nil and propertyReads.Rotation == nil and propertyReads.Orientation == nil
			and propertyReads.BrickColor == nil and propertyReads.Color3uint8 == nil, "read alias after canonical getter succeeded")
		assert(propertyReads.AssemblyLinearVelocity == nil and propertyReads.CurrentPhysicalProperties == nil,
			"read a property the change tracker unconditionally ignores")
		assert(propertyReads.Flag == 1 and propertyReads.Unset == 1)
		for _, name in { "CFrame", "Position", "Orientation", "Rotation", "Color", "BrickColor", "Flag", "Unset" } do
			testGame.ItemChanged:Fire(part, name)
		end
	end
	assert(fallbackReads.TextureContent == 1 and fallbackReads.Texture == 1, "lost or repeated raw alias fallback")
	assert(#tracker.drainChangeJournal("canonical-baselines") == 0, "first unchanged alias became an outside edit")
	-- Actual unreadable events remain guarded even when priming found a raw fallback.
	testGame.ItemChanged:Fire(fallbackBeam, "Texture")
	local unreadableRecords = tracker.drainChangeJournal("canonical-baselines")
	assert(#unreadableRecords == 1 and unreadableRecords[1].instance == fallbackBeam
		and unreadableRecords[1].properties.TextureContent.captured == false)
	for _, part in parts do
		local values = valuesByPart[part]
		values.CFrame = roblox.CFrame.new(4, 5, 6)
		values.Color = roblox.Color3.new(0.5, 0.6, 0.7)
		values.Flag = true
		values.Unset = "set"
		for _, name in { "Position", "CFrame", "BrickColor", "Color", "Flag", "Unset" } do
			testGame.ItemChanged:Fire(part, name)
		end
	end
	local records = tracker.drainChangeJournal("canonical-baselines")
	assert(#records == 100, "canonical baseline deduplication lost a real edit")
	for _, record in records do
		assert(record.properties.CFrame.value == roblox.CFrame.new(4, 5, 6)
			and record.properties.Color.value == roblox.Color3.new(0.5, 0.6, 0.7)
			and record.properties.Flag.value == true and record.properties.Unset.value == "set")
	end
	tracker.beginSuppress()
	for i, part in parts do
		local target = roblox.CFrame.new(i, 10, 20)
		local token = tracker.expectPropertyEvent(part, "CFrame", target)
		valuesByPart[part].CFrame = target
		for _, name in if i % 2 == 0 then { "Position", "Orientation", "CFrame", "Rotation" }
			else { "CFrame", "Rotation", "Position", "Orientation" } do
			testGame.ItemChanged:Fire(part, name)
		end
		assert(not token.active, "alias-first own write did not consume its exact canonical expectation")
	end
	assert(#tracker.drainChangeJournal("canonical-baselines") == 0, "own CFrame alias became an outside edit")
	for _, part in parts do
		local cancelled = tracker.expectPropertyEvent(part, "CFrame", roblox.CFrame.new(50, 60, 70))
		tracker.cancelExpectedEvent(cancelled)
		valuesByPart[part].CFrame = roblox.CFrame.new(50, 60, 70)
		testGame.ItemChanged:Fire(part, "Orientation")
	end
	local cancelledRecords = tracker.drainChangeJournal("canonical-baselines")
	assert(#cancelledRecords == 100)
	for _, record in cancelledRecords do
		assert(record.properties.CFrame.value == roblox.CFrame.new(50, 60, 70) and record.properties.Orientation == nil)
	end
	local first = parts[1]
	local expected = tracker.expectPropertyEvent(first, "CFrame", roblox.CFrame.new(80, 90, 100))
	valuesByPart[first].CFrame = roblox.CFrame.new(1, 2, 3)
	testGame.ItemChanged:Fire(first, "Rotation")
	local outsideRecords = tracker.drainChangeJournal("canonical-baselines")
	assert(expected.active and #outsideRecords == 1
		and outsideRecords[1].properties.CFrame.value == roblox.CFrame.new(1, 2, 3))
	tracker.cancelExpectedEvent(expected)
	tracker.endSuppress(0)
	tracker.stop()
	remove(root)
end
print("Canonical priming: 100 first-event/alias edits, single reads, ignored fields, nil/false and unavailable canonical getter passed")

do
	tracker = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
	tracker.configurePropertyCandidates({ Part = { "Flag", "Unset", "Label" } })
	tracker.getState({ reset = true })
	local root = instance("Folder", "BaselineSchemaRefresh")
	local old = instance("Part", "Old", root)
	old.Flag, old.Label = false, "original"
	tracker.beginSuppress()
	tracker.expectParentChange(root, storage)
	add(root, storage, true)
	tracker.endSuppress(0)
	tracker.configurePropertyCandidates({ Part = { "NewField", "Label", "Unset", "Flag" } })
	local fresh = instance("Part", "New")
	fresh.Flag, fresh.Label, fresh.NewField = true, "fresh", 42
	tracker.beginSuppress()
	tracker.expectParentChange(fresh, root)
	add(fresh, root, true)
	tracker.endSuppress(0)
	tracker.beginChangeJournal("baseline-schema-refresh", { "ReplicatedStorage" })
	for _, target in { old, fresh } do
		for _, property in { "Flag", "Unset", "Label" } do testGame.ItemChanged:Fire(target, property) end
	end
	testGame.ItemChanged:Fire(fresh, "NewField")
	assert(#tracker.drainChangeJournal("baseline-schema-refresh") == 0, "schema refresh changed a captured baseline")
	old.Flag, old.Unset, fresh.Label = true, "new value", "changed"
	testGame.ItemChanged:Fire(old, "Flag")
	testGame.ItemChanged:Fire(old, "Unset")
	testGame.ItemChanged:Fire(fresh, "Label")
	local records = tracker.drainChangeJournal("baseline-schema-refresh")
	assert(#records == 2, "schema refresh lost a real first edit")
	tracker.stop()
	remove(root)
end
print("Property baseline schema refresh: existing/new instances, nil/false and first edits preserved")

tracker = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
tracker.getState({ reset = true })
local function waiting(lease)
	local result
	local thread = coroutine.create(function()
		result = tracker.getState({ start = false, compact = true, waitSeconds = 25 }, lease)
	end)
	assert(coroutine.resume(thread))
	assert(coroutine.status(thread) == "suspended")
	return function(cancelled)
		if cancelled then
			assert(coroutine.status(thread) == "dead" and result.waitCancelled and not result.waitTimedOut)
		else
			assert(coroutine.status(thread) == "suspended" and result == nil)
		end
	end
end
for cycle = 1, 100 do
	local first, sameLease, other = waiting("old"), waiting("old"), waiting("new")
	tracker.cancelWait("unrelated")
	first(false) sameLease(false) other(false)
	tracker.cancelWait("old")
	first(true) sameLease(true) other(false)
	tracker.cancelWait("old")
	other(false)
	tracker.cancelWait()
	other(true)
end
tracker.stop()
assert(not tracker.getState({ start = false }).tracking, "observer restarted stopped tracking")
print("Studio wait cancellation: 100 lease-isolated cycles, multiple waits, global cancel and stopped observer passed")

do
	local source = fs.readFile("tools/plugin_ws_bridge/BridgeStudioChanges.module.lua")
	local function section(first, after)
		local start = assert(source:find(first, 1, true))
		return source:sub(start, assert(source:find(after, start, true)) - 1)
	end
	local names, keys, plans, relevance, api = {}, {}, {}, {}, {}
	local readName = luau.load(
		section("\tlocal function propertyCacheKey(", "\tlocal function propertyValueFingerprint(")
			.. section("\tfunction api.configurePropertyCandidates(", "\tfunction api.setConflictResolution(")
			.. "\nreturn propertyReadNameForEvent", { environment = {
			state = {}, api = api, propertyReadNamesByClass = names, propertyCacheKeysByClass = keys,
			propertyPrimingPlansByClass = plans, propertyEventRelevanceByClass = relevance,
			refreshValuePropertySignals = function() end,
			ATTRIBUTE_EVENT_PROPERTIES = {}, exportPropertyNameForEvent = function(_, name) return name end,
			RbxDomModule = { getContentPropertyAliases = function() return {} end },
		} }
	)()
	local target = { ClassName = "Folder", IsA = function() return false end }
	api.configurePropertyCandidates({ Folder = { "Label" } })
	assert(readName(target, "label") == "Label" and names.Folder.label == "Label")
	plans.Folder = { { "Label", "Label", "property:label" } }
	relevance.Folder = { label = true }
	api.configurePropertyCandidates({ Folder = { "LABEL" } })
	assert(next(plans) == nil, "schema refresh retained a priming plan")
	assert(next(relevance) == nil, "schema refresh retained property relevance")
	assert(readName(target, "label") == "LABEL", "schema refresh retained a cached property name")
	plans.Folder = { { "LABEL", "LABEL", "property:label" } }
	api.configurePropertyCandidates(nil)
	assert(next(plans) == nil, "schema removal retained a priming plan")
	assert(readName(target, "label") == "label", "schema removal retained a cached property name")
end
print("Property-name caches: candidate refresh/removal invalidates class mappings")

do
	local service = instance("ServerStorage", "ServerStorage", testGame)
	services.ServerStorage = service
	local child = instance("Folder", "Observed", service)
	local rawConfig = table.clone(trackerConfig)
	local rawTracker = trackerModule.create(rawConfig, { ServerStorage = true })
	assert(rawTracker.attributeObservation("ServerStorage") == nil)
	rawTracker.getState({ services = { "ServerStorage" } })
	local observation = assert(rawTracker.attributeObservation("ServerStorage"))
	assert(observation.connections[child] == "ServerStorage")
	assert(rawTracker.attributeObservation("ServerStorage") == observation)
	local value = 0
	function child:GetAttribute() return value end
	function service:GetAttribute() return value end
	for _, target in { child, service } do
		for _, mode in { "filtered", "expected", "ordinary" } do
			local before = observation.generation
			value += 1
			rawTracker.setOptions({ syncbackProperties = mode ~= "filtered" })
			local token = if mode == "expected" then rawTracker.expectAttributeEvent(target, "Probe", value) else nil
			target.AttributeChanged:Fire("Probe")
			assert(observation.generation == before + 1 and observation.instance == target and observation.attribute == "Probe")
			if token then rawTracker.cancelExpectedEvent(token) end
		end
	end
	-- Removing a listener invalidates old checkpoints, even if tracking resumes
	-- before an exporter checks them. The replacement must not revive the token.
	child.Parent = nil
	service.DescendantRemoving:Fire(child)
	assert(not observation.active)
	local nextObservation = assert(rawTracker.attributeObservation("ServerStorage"))
	assert(nextObservation ~= observation and nextObservation.active)
	rawTracker.stop()
	assert(not nextObservation.active and rawTracker.attributeObservation("ServerStorage") == nil)
	rawTracker.getState({ services = { "ServerStorage" } })
	assert(rawTracker.attributeObservation("ServerStorage") ~= nextObservation)
	rawTracker.setOptions({ onlyCodeMode = true })
	assert(rawTracker.attributeObservation("ServerStorage") == nil)
	rawTracker.stop()
end
print("Raw attribute observations: filtered/expected/root events, listener loss and restart preserve export guards")

-- Exercise the actual tracker with the existing instance/signal fixtures. Force
-- the foreign callback first: the fixture's ordinary signal map is unordered.
do
	local savedItemChanged = testGame.ItemChanged
	for _, useItemChanged in { true, false } do
		testGame.ItemChanged = if useItemChanged then signal() else nil
		local function propertyEvent(target, name)
			if useItemChanged then testGame.ItemChanged:Fire(target, name)
			elseif target:IsA("ValueBase") then target:GetPropertyChangedSignal(name):Fire()
			else target.Changed:Fire(name) end
		end
		for _, delayedAdds in { false, true } do
			for _, mode in { "outside", "outside-then-own", "own-only" } do
			for _, serializedInsertion in { false, true } do
				local root = instance("Folder", "PreExposure")
				local first = instance("Part", "Duplicate", root)
				local later = instance("Part", "Duplicate", root)
				later.Transparency = 0.25
				local attributes = { Update = "before", Delete = "before" }
				local attributeReads = 0
				function later:GetAttributes() attributeReads += 1 return table.clone(attributes) end
				function later:GetAttribute(name) return attributes[name] end
				local observer = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
				observer.configurePropertyCandidates({ Part = { "Transparency" } })
				local fire = storage.DescendantAdded.Fire
				local queued, laterDelivered, foreignCalls = {}, false, 0
				storage.DescendantAdded.Fire = function(self, target)
					local function deliver()
						if target == first then
							foreignCalls += 1
							assert(not laterDelivered, "foreign edit must precede the later node's add callback")
							if not serializedInsertion then
								propertyEvent(later, "Transparency") -- no-op control at the original value
							end
							if mode ~= "own-only" then
								later.Transparency = 0.5
								propertyEvent(later, "Transparency")
								attributes.Add, attributes.Update, attributes.Delete = "foreign", "foreign", nil
								for _, name in { "Add", "Update", "Delete" } do later.AttributeChanged:Fire(name) end
							end
							if mode ~= "outside" then
								observer.expectPropertyEvent(later, "Transparency", 0.75)
								later.Transparency = 0.75
								propertyEvent(later, "Transparency")
								for _, name in { "Add", "Update", "Delete" } do
									observer.expectAttributeEvent(later, name, "own")
									attributes[name] = "own"
									later.AttributeChanged:Fire(name)
								end
							end
						end
						fire(self, target)
						if target == later then laterDelivered = true end
					end
					if delayedAdds then table.insert(queued, deliver) else deliver() end
				end
				observer.getState({ services = { "ReplicatedStorage" } })
				observer.beginSuppress()
				observer.beginChangeJournal("pre-exposure", { "ReplicatedStorage" })
				local before, profile = observer.serviceGeneration("ReplicatedStorage"), {}
				local token = observer.expectParentChange(root, storage, profile, serializedInsertion)
				assert(attributeReads == (if serializedInsertion then 0 else 1),
					"finished native payload eagerly copied attribute baselines")
				assert(later.AttributeChanged:Count() == 1 and profile.primedInstances == 3,
					"fresh full-mode subtree must be observed before exposure")
				add(root, storage)
				propertyEvent(root, "Parent")
				local complete = assert(token.complete)
				if mode == "own-only" and not delayedAdds then observer.cancelExpectedEvent(token) end
				complete() complete()
				assert(token.complete == nil and later.AttributeChanged:Count() == 1,
					"completion/cancellation must retain an acquired observer once it is live")
				-- Delay structural delivery past complete(), not property sampling. This
				-- checks deferred expectation lifetime, not a simulated engine scheduler.
				for _, deliver in queued do deliver() end
				assert(foreignCalls == 1 and laterDelivered and profile.primedInstances == 3,
					"DescendantAdded must not re-prime an already prepared observer")
				for _, expected in token.tokens do assert(not expected.active, "own structural/Parent event was not consumed") end
				local records = observer.finishChangeJournal("pre-exposure")
				if mode == "own-only" then
					assert(#records == 0 and observer.serviceGeneration("ReplicatedStorage") == before,
						"own attachment, setters and unchanged property events must produce zero self-journal entries")
				else
					assert(#records == 1 and records[1].instance == later and not records[1].structural)
					local record = records[1]
					assert(record.pathOrdinals[#record.pathOrdinals] == 2, "duplicate identity was lost")
					assert(record.properties.Transparency.captured and record.properties.Transparency.value == 0.5)
					assert(record.attributes.Add.captured and record.attributes.Add.value == "foreign")
					assert(record.attributes.Update.captured and record.attributes.Update.value == "foreign")
					assert(record.attributes.Delete.captured and record.attributes.Delete.value == nil,
						"own overwrite must retain the observed outside deletion")
					assert(observer.serviceGeneration("ReplicatedStorage") == before + 4)
					assert(later.Transparency == (if mode == "outside" then 0.5 else 0.75))
				end
				observer.endSuppress(0)
				observer.stop() observer.stop()
				assert(later.AttributeChanged:Count() == 0 and later.Changed:Count() == 0)
				storage.DescendantAdded.Fire = fire
				remove(root)
			end
			end
		end

		for _, failure in { "cancel", "attributes", "attribute-connect", "parent-connect" } do
			if useItemChanged and failure == "parent-connect" then continue end
			local prior = instance("StringValue", "PriorObserver", storage)
			prior.Value = "before"
			local reads = 0
			function prior:GetAttributes() reads += 1 return {} end
			local observer = trackerModule.create(trackerConfig, { ReplicatedStorage = true, Workspace = true })
			observer.configurePropertyCandidates({ StringValue = { "Value" } })
			observer.getState({ services = { "ReplicatedStorage", "Workspace" } })
			propertyEvent(prior, "Value")
			deferred = {}
			remove(prior) -- old observer survives until the queued removal cleanup
			local root = instance("Folder", "CancelledPreExposure")
			prior.Parent = root
			table.insert(root.children, prior)
			local fresh = instance("StringValue", "Fresh", root)
			fresh.Value = "before"
			local readsBefore = reads
			local observation = assert(observer.attributeObservation("ReplicatedStorage"))
			local getAttributes, connectAttributes = fresh.GetAttributes, fresh.AttributeChanged.Connect
			local connectParent = fresh:GetPropertyChangedSignal("Parent").Connect
			local fail = function() error("preconnect setup fault", 0) end
			if failure == "attributes" then fresh.GetAttributes = fail
			elseif failure == "attribute-connect" then fresh.AttributeChanged.Connect = fail
			elseif failure == "parent-connect" then fresh:GetPropertyChangedSignal("Parent").Connect = fail end
			observer.beginSuppress()
			local ok, token = pcall(observer.expectParentChange, root, workspace)
			if failure == "cancel" then
				assert(ok and fresh.AttributeChanged:Count() == 1)
				local complete = assert(token.complete)
				observer.cancelExpectedEvent(token) observer.cancelExpectedEvent(token)
				complete(true) complete(true)
				assert(token.complete == nil)
				for _, expected in token.tokens do assert(not expected.active) end
			else
				assert(not ok and tostring(token):find("preconnect setup fault", 1, true), "original setup failure must propagate")
			end
			assert(root.AttributeChanged:Count() == 0 and root.Changed:Count() == 0)
			assert(fresh.AttributeChanged:Count() == 0 and fresh:GetPropertyChangedSignal("Value"):Count() == 0
				and fresh:GetPropertyChangedSignal("Parent"):Count() == 0, `partial {failure} setup leaked a listener`)
			assert(prior.AttributeChanged:Count() == 1 and prior:GetPropertyChangedSignal("Value"):Count() == (if useItemChanged then 0 else 1)
				and observation.connections[prior] == "ReplicatedStorage" and reads == readsBefore,
				"cancelled cross-service preparation must not replace or re-prime a prior observer")
			-- Re-expose without another expectation: cancelled/failed preparation
			-- must not swallow a later outside add or its first real property edit.
			local before = observer.serviceGeneration("Workspace")
			fresh.GetAttributes = getAttributes
			fresh.AttributeChanged.Connect = connectAttributes
			fresh:GetPropertyChangedSignal("Parent").Connect = connectParent
			observer.beginChangeJournal("cancelled-pre-exposure", { "Workspace" })
			add(root, workspace)
			propertyEvent(prior, "Value")
			local records = observer.drainChangeJournal("cancelled-pre-exposure")
			assert(#records == 3 and observer.serviceGeneration("Workspace") > before,
				"failed preparation left active own-add expectations")
			fresh.Value = "outside"
			propertyEvent(fresh, "Value")
			records = observer.finishChangeJournal("cancelled-pre-exposure")
			assert(#records == 1 and records[1].instance == fresh and records[1].properties.Value.value == "outside")
			flushDeferred()
			deferred = nil
			observer.endSuppress(0)
			observer.stop() observer.stop()
			assert(prior.AttributeChanged:Count() == 0 and fresh:GetPropertyChangedSignal("Value"):Count() == 0)
			remove(root)
		end
	end
	testGame.ItemChanged = savedItemChanged
end
print("Pre-exposure tracking: event-time outside edits/own overwrites, deferred adds, cancellation, partial setup and prior observers passed")

do
	local savedItemChanged = testGame.ItemChanged
	for _, shared in { true, false } do
		for _, initiallyDetached in { true, false } do
			for _, reattach in { true, false } do
				testGame.ItemChanged = if shared then signal() else nil
				local root = instance("Folder", "DetachedJournal", if initiallyDetached then nil else storage)
				local child = instance("Part", "Child", root)
				child.Transparency = 0.25
				local attributes = { Keep = "before", Delete = "before" }
				function child:GetAttributes() return table.clone(attributes) end
				function child:GetAttribute(name) return attributes[name] end
				local observer = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
				observer.configurePropertyCandidates({ Part = { "Transparency" } })
				observer.getState({ services = { "ReplicatedStorage" } })
				observer.beginSuppress()
				observer.beginChangeJournal("detached", { "ReplicatedStorage" })
				local parentToken
				if initiallyDetached then
					parentToken = observer.expectParentChange(root, storage)
				else
					observer.expectParentChange(root, nil)
					deferred = {}
					local fire = storage.DescendantRemoving.Fire
					storage.DescendantRemoving.Fire = function(self, target)
						if target == root then
							child.Transparency = 0.4
							child.Changed:Fire("Transparency")
						end
						fire(self, target)
					end
					remove(root)
					storage.DescendantRemoving.Fire = fire
					flushDeferred()
					deferred = nil
					local records = observer.drainChangeJournal("detached")
					local found
					for _, record in records do if record.instance == child then found = record end end
					assert(found and found.properties.Transparency.value == 0.4,
						"an earlier removal callback bypassed the local transaction observer")
				end
				assert(child.Changed:Count() == 1 and child.AttributeChanged:Count() == 1,
					"detached transaction object lost its local observers")
				-- ItemChanged is deliberately not fired: real Studio does not emit it here.
				child.Transparency = 0.5
				child.Changed:Fire("Transparency")
				attributes.Keep, attributes.Delete = "outside", nil
				child.AttributeChanged:Fire("Keep") child.AttributeChanged:Fire("Delete")
				observer.expectPropertyEvent(child, "Transparency", 0.75)
				child.Transparency = 0.75
				child.Changed:Fire("Transparency")
				for _, name in { "Keep", "Delete" } do
					observer.expectAttributeEvent(child, name, "own")
					attributes[name] = "own"
					child.AttributeChanged:Fire(name)
				end
				if reattach then
					parentToken = parentToken or observer.expectParentChange(root, storage)
					add(root, storage)
					if shared then testGame.ItemChanged:Fire(root, "Parent") else root.Changed:Fire("Parent") end
					if parentToken.complete then parentToken.complete() end
					assert(child.Changed:Count() == (if shared then 0 else 1), "temporary listener survived reattachment")
				end
				local records = observer.finishChangeJournal("detached")
				local found
				for _, record in records do if record.instance == child then found = record end end
				assert(found and found.properties.Transparency.value == 0.5)
				assert(found.attributes.Keep.value == "outside" and found.attributes.Delete.captured
					and found.attributes.Delete.value == nil, "own detached writes replaced the outside journal")
				if not reattach then
					assert(child.Changed:Count() == 0 and child.AttributeChanged:Count() == 0,
						"finished detached journal leaked observers")
				end
				if parentToken then observer.cancelExpectedEvent(parentToken) end
				observer.endSuppress(0)
				observer.stop()
				if root.Parent then remove(root) end
			end
		end
	end
	testGame.ItemChanged = savedItemChanged
end
print("Detached journals: pre-attachment/removal edits, own overwrites, deferred cleanup, reattachment and finish cleanup passed")

do
	local savedItemChanged = testGame.ItemChanged
	local savedDescriptor = dependencies.RbxDom.findCanonicalPropertyDescriptor
	dependencies.RbxDom.findCanonicalPropertyDescriptor = function(class, name)
		return if class == "StringValue" and name == "Sandboxed" then { scriptability = "ReadWrite" }
			else savedDescriptor(class, name)
	end
	for _, shared in { true, false } do
		testGame.ItemChanged = if shared then signal() else nil
		local root = instance("Folder", "DetachedValueJournal", storage)
		local target = instance("StringValue", "Before", root)
		local destination = instance("Folder", "DetachedDestination", root)
		target.Value = "before"
		local observer = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
		observer.configurePropertyCandidates({ StringValue = { "Value", "Archivable" } })
		observer.getState({ services = { "ReplicatedStorage" } })
		observer.beginSuppress()
		observer.beginChangeJournal("detached-value", { "ReplicatedStorage" })
		local token = observer.expectParentChange(root, nil)
		remove(root)
		observer.configurePropertyCandidates({ StringValue = { "Value", "Archivable", "Sandboxed" } })
		observer.configurePropertyCandidates({ StringValue = { "Sandboxed", "Archivable", "Value" } })
		assert(target:GetPropertyChangedSignal("Sandboxed"):Count() == 1, "schema refresh did not update the existing ValueBase listener")
		observer.configurePropertyCandidates({ StringValue = { "Value", "Archivable" } })
		assert(target:GetPropertyChangedSignal("Sandboxed"):Count() == 0, "schema removal retained an obsolete ValueBase listener")
		observer.configurePropertyCandidates({ StringValue = { "Value", "Archivable", "Sandboxed" } })
		assert(target:GetPropertyChangedSignal("Sandboxed"):Count() == 1, "unchanged schema lost the next ValueBase refresh")
		for _, change in { { "Name", "Outside" }, { "Archivable", false }, { "Parent", destination }, { "Sandboxed", false } } do
			local name, value = change[1], change[2]
			target[name] = value
			-- ValueBase.Changed does not report these properties, and a detached
			-- object emits no DataModel.ItemChanged. Use the real signal contract.
			target:GetPropertyChangedSignal(name):Fire()
			observer.expectPropertyEvent(target, name, if name == "Parent" then root else if name == "Name" then "Own" else true)
			target[name] = if name == "Parent" then root else if name == "Name" then "Own" else true
			target:GetPropertyChangedSignal(name):Fire()
		end
		local found
		for _, record in observer.finishChangeJournal("detached-value") do
			if record.instance == target then found = record end
		end
		assert(found and found.properties.Name and found.properties.Name.value == "Outside",
			"detached ValueBase rename was lost")
		assert(found.properties.Archivable and found.properties.Archivable.value == false,
			"detached ValueBase Archivable edit was lost")
		assert(found.properties.Parent and found.properties.Parent.value == destination,
			"detached ValueBase move was lost")
		assert(found.properties.Sandboxed and found.properties.Sandboxed.value == false,
			"refreshed ValueBase property was lost")
		observer.cancelExpectedEvent(token)
		observer.endSuppress(0)
		observer.stop()
		for _, name in { "Name", "Parent", "Archivable", "Sandboxed" } do
			assert(target:GetPropertyChangedSignal(name):Count() == 0, "ValueBase fallback leaked a connection")
		end
	end
	testGame.ItemChanged = savedItemChanged
	dependencies.RbxDom.findCanonicalPropertyDescriptor = savedDescriptor
end
print("Detached ValueBase journals: outside names, parents and Archivable survive own overwrites and release all listeners")

do
	local savedItemChanged = testGame.ItemChanged
	for _, persistent in { false, true } do
		testGame.ItemChanged = signal()
		local observer = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
		observer.configurePropertyCandidates({ Part = { "Transparency" } })
		if persistent then observer.getState({ services = { "ReplicatedStorage" } }) end
		observer.getState({ services = { "ReplicatedStorage" }, trackingGuardId = "push-completion" })
		moving.Transparency = 0.2
		testGame.ItemChanged:Fire(moving, "Transparency")
		local accepted = observer.getState({ start = false })
		moving.Transparency = 0.4
		testGame.ItemChanged:Fire(moving, "Transparency")
		local request = { services = { "ReplicatedStorage" }, start = false,
			ackSeq = accepted.seq, runtimeId = "test", releaseTrackingGuardId = "push-completion" }
		for _ = 1, 2 do
			local result = observer.getState(request)
			assert(result.tracking == persistent and result.persistentTracking == persistent)
			assert((result.connectedInstances > 0) == persistent)
			assert(result.changeCount > 0 and result.seq > accepted.seq,
				"Combined completion acknowledged an edit newer than its accepted snapshot")
		end
		observer.stop()
	end
	testGame.ItemChanged = savedItemChanged
end
print("Push completion: combined acknowledgment/release retains newer edits and persistent tracking; repeated completion is safe")

do
	local savedItemChanged = testGame.ItemChanged
	for _, mode in { "unarm", "persistent", "release", "own" } do
		for cycle = 1, 25 do
			testGame.ItemChanged = signal()
			local observer = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
			local state = observer.getState({ services = { "ReplicatedStorage" }, trackingGuardId = "relay",
				nativeAttributeRelay = true })
			local relay = createdRelays[#createdRelays]
			assert(state.trackingStarted and state.nativeAttributeRelay[1] == "CoreGui")
			relay.Changed:Fire(relay)
			services.RunService.Heartbeat:Fire()
			assert(relay.Frame == 1, "native frame observation did not start")
			observer.beginChangeJournal("relay", { "ReplicatedStorage" })
			local profile = {}
			observer.beginNativeImportObservations("relay", profile, if cycle % 2 == 0 then { Folder = true } else nil)
			local fresh = instance("Folder", "RelayIncoming", storage)
			function fresh:GetDebugId() return "relay-fresh" end
			local attributes, reads = { Keep = false, Removed = 7 }, 0
			function fresh:GetAttributes() reads += 1 return table.clone(attributes) end
			function fresh:GetAttribute(name) return attributes[name] end
			storage.DescendantAdded:Fire(fresh)
			local parentGeneration = observer.serviceGeneration("ReplicatedStorage")
			testGame.ItemChanged:Fire(fresh, "Parent")
			assert(observer.serviceGeneration("ReplicatedStorage") == parentGeneration,
				"native receipt did not supply the initial parent baseline")
			assert(reads == 0 and fresh.AttributeChanged:Count() == 0 and profile.nativeAttributeInstances == 1,
				"transient native insertion eagerly primed attributes or installed a local listener")
			attributes.Removed = nil
			relay.Changed:Fire(fresh)
			observer.finishNativeImportObservations("relay", { ["relay-fresh"] = "Folder" })
			local records = observer.drainChangeJournal("relay")
			assert(#records == 1 and records[1].attributesSnapshot.Keep == false
				and records[1].attributesSnapshot.Removed == nil, "native snapshot lost false or deletion")
			testGame.ItemChanged:Fire(fresh, "Parent")
			assert(#observer.drainChangeJournal("relay") == 0, "native receipt baseline was lost after import completion")
			if mode == "unarm" then
				relay.Changed:Fire(nil)
				services.RunService.Heartbeat:Fire()
				assert(relay.Frame == 1, "unarmed relay continued native frame signaling")
			elseif mode == "persistent" then
				observer.getState({ services = { "ReplicatedStorage" } })
			elseif mode == "release" then
				observer.getState({ start = false, releaseTrackingGuardId = "relay" })
			else
				observer.beginSuppress()
				local generation = observer.serviceGeneration("ReplicatedStorage")
				observer.expectAttributeEvent(fresh, "Own", 12)
				attributes.Own = 12
				fresh.AttributeChanged:Fire("Own")
				relay.Changed:Fire(fresh)
				assert(observer.serviceGeneration("ReplicatedStorage") == generation, "promoted own attribute became a conflict")
				observer.endSuppress(0)
			end
			assert(fresh.AttributeChanged:Count() == (if mode == "release" then 0 else 1),
				"native observer transition lost or duplicated the local listener")
			if mode ~= "release" then
				attributes.After = 19
				fresh.AttributeChanged:Fire("After")
				local later = observer.drainChangeJournal("relay")
				assert(#later == 1 and later[1].attributes.After.value == 19, "transition lost a later attribute edit")
			end
			observer.stop()
			assert(relay.destroyed and relay.Changed:Count() == 0 and services.RunService.Heartbeat:Count() == 0)
			assert(fresh.AttributeChanged:Count() == 0, "stopped observer leaked a promoted connection")
			table.remove(storage.children, table.find(storage.children, fresh))
		end
	end
	testGame.ItemChanged = savedItemChanged
end
print("Native attribute journal: 100 cancellation/persistent/lease/own-write transitions, deletion snapshots and listener cleanup passed")

do
	local savedItemChanged = testGame.ItemChanged
	for _, mode in { "native", "fallback", "cancel", "persistent" } do
		for _ = 1, 25 do
			testGame.ItemChanged = signal()
			local existing = instance("Folder", "RelayExisting", storage)
			local attributes = { Keep = false, Removed = 7 }
			function existing:GetAttributes() return table.clone(attributes) end
			function existing:GetAttribute(name) return attributes[name] end
			local observer = trackerModule.create(trackerConfig, { ReplicatedStorage = true, Workspace = true })
			local request = { services = { "ReplicatedStorage" }, trackingGuardId = "deferred-relay",
				nativeAttributeRelay = true, deferNativeTracking = true }
			local pending = observer.getState(request)
			local relay = createdRelays[#createdRelays]
			assert(pending.nativeTrackingDeferred and existing.AttributeChanged:Count() == 0)
			assert(not observer.getState({ start = false }).tracking)
			if mode == "cancel" then
				observer.getState({ start = false, releaseTrackingGuardId = "deferred-relay" })
				relay.Changed:Fire(relay)
				assert(not observer.getState({ start = false }).tracking,
					"a late native handshake restarted a cancelled lease")
			else
				if mode == "persistent" then
					observer.getState({ services = { "ReplicatedStorage" } })
				end
				if mode ~= "fallback" then relay.Changed:Fire(relay) end
				local state = observer.getState({ services = request.services, trackingGuardId = request.trackingGuardId,
					includeGenerations = true })
				assert(state.tracking and existing.AttributeChanged:Count() == (if mode == "native" then 0 else 1))
				observer.getState({ services = { "Workspace" }, trackingGuardId = "other-service" })
				assert(workspace.AttributeChanged:Count() == 1,
					"A service outside the native observation scope lost its local listener")
				observer.beginChangeJournal("deferred-relay", request.services)
				attributes.Removed = nil
				if mode == "native" then relay.Changed:Fire(existing) else existing.AttributeChanged:Fire("Removed") end
				local records = observer.drainChangeJournal("deferred-relay")
				assert(#records == 1, "deferred tracking lost the first outside attribute deletion")
				if mode == "native" then
					assert(records[1].attributesSnapshot.Keep == false and records[1].attributesSnapshot.Removed == nil)
					relay.Changed:Fire(nil)
					assert(existing.AttributeChanged:Count() == 1, "native cancellation lost existing-instance observation")
					attributes.After = false
					existing.AttributeChanged:Fire("After")
					assert(observer.drainChangeJournal("deferred-relay")[1].attributes.After.value == false)
				end
			end
			observer.stop()
			assert(relay.destroyed and relay.Changed:Count() == 0 and services.RunService.Heartbeat:Count() == 0)
			assert(existing.AttributeChanged:Count() == 0)
			table.remove(storage.children, table.find(storage.children, existing))
		end
	end
	testGame.ItemChanged = savedItemChanged
end
print("Native tracking startup: 100 existing-instance, fallback, cancelled-lease and persistent transitions retain first edits and clean up")
do
	local collection = services.CollectionService
	local saved = table.clone(collection)
	local tagged = false
	local added, removed = signal(), signal()
	collection.GetTags = function(_, target) return if target == moving and tagged then { "Immediate" } else {} end
	collection.GetAllTags = function() return if tagged then { "Immediate" } else {} end
	collection.GetTagged = function() return if tagged then { moving } else {} end
	collection.GetInstanceAddedSignal = function() return added end
	collection.GetInstanceRemovedSignal = function() return removed end
	testGame.ItemChanged = signal()
	local observer = trackerModule.create(trackerConfig, { ReplicatedStorage = true })
	observer.getState({ services = { "ReplicatedStorage" }, reset = true })
	for iteration = 1, 100 do
		local before = observer.serviceGeneration("ReplicatedStorage")
		tagged = true
		collection.TagAdded:Fire("Immediate")
		local after = observer.serviceGeneration("ReplicatedStorage")
		assert(after > before, "new tag waited for polling before advancing the transaction guard")
		added:Fire(moving)
		assert(observer.serviceGeneration("ReplicatedStorage") == after, "first tag membership was reported twice")
		tagged = false
		if iteration % 2 == 0 then removed:Fire(moving) end
		collection.TagRemoved:Fire("Immediate")
		local final = observer.serviceGeneration("ReplicatedStorage")
		assert(final > after, "last tag membership was lost when global removal arrived first")
		removed:Fire(moving)
		assert(observer.serviceGeneration("ReplicatedStorage") == final)
		assert(added:Count() == 0 and removed:Count() == 0, "retired tag listeners leaked")
	end
	observer.stop()
	assert(collection.TagAdded:Count() == 0 and collection.TagRemoved:Count() == 0)
	table.clear(collection)
	for key, value in pairs(saved) do collection[key] = value end
end
print("Tag discovery: 100 immediate first/last memberships, both removal orders, reuse and listener cleanup passed")

do
	local savedItemChanged = testGame.ItemChanged
	local savedCapture = dependencies.BridgeScriptDocuments.capture
	local collection = services.CollectionService
	local savedCollection = table.clone(collection)
	local tagAdded, tagRemoved = signal(), signal()
	local documentChanged = signal()
	collection.GetAllTags = function() return { "ProofTag" } end
	collection.GetTagged = function() return { localScript } end
	collection.GetInstanceAddedSignal = function() return tagAdded end
	collection.GetInstanceRemovedSignal = function() return tagRemoved end
	for _, mode in { "attribute", "property", "document", "close", "unarm", "stop", "restart", "persistent", "tag-added", "tag-removed", "member-added", "member-removed" } do
		for _ = 1, 20 do
			testGame.ItemChanged = signal()
			local contentGeneration = 0
			local documents = { { instance = localScript, source = "original" } }
			dependencies.BridgeScriptDocuments.capture = function() return documents end
			local proofConfig = table.clone(trackerConfig)
			proofConfig.exportProofGeneration = function() return contentGeneration end
			proofConfig.observeProofDocuments = function(changed) return { documentChanged:Connect(changed) } end
			local observer = trackerModule.create(proofConfig, { ReplicatedStorage = true })
			observer.getState({ services = { "ReplicatedStorage" }, trackingGuardId = "proof",
				nativeAttributeRelay = true, deferNativeTracking = true })
			local relay = createdRelays[#createdRelays]
			relay.Changed:Fire(relay)
			local proof = assert(observer.capturePushProof())
			assert(observer.verifyPushProof(proof) and not observer.verifyPushProof(proof + 1))
			local released = observer.getState({ start = false, releaseTrackingGuardId = "proof", retainPushProof = proof })
			assert(not released.tracking and released.retainedPushProof == proof and not relay.destroyed)
			assert(localScript.AttributeChanged:Count() == 0, "cached equality retained per-instance listeners")
			assert(observer.getState({ start = false, verifyPushProof = proof }).pushProofMatches)
			if mode == "attribute" then relay.Changed:Fire(localScript)
			elseif mode == "property" then contentGeneration += 1
			elseif mode == "document" then documents[1].source = "outside edit"
			elseif mode == "close" then documents = {}
			elseif mode == "tag-added" then services.CollectionService.TagAdded:Fire("New")
			elseif mode == "tag-removed" then services.CollectionService.TagRemoved:Fire("Old")
			elseif mode == "member-added" then tagAdded:Fire(moving)
			elseif mode == "member-removed" then tagRemoved:Fire(localScript)
			elseif mode == "unarm" then relay.Changed:Fire(nil)
			elseif mode == "restart" then
				observer.getState({ services = { "ReplicatedStorage" }, trackingGuardId = "new-host",
					nativeAttributeRelay = true, deferNativeTracking = true })
				local nextRelay = createdRelays[#createdRelays]
				assert(nextRelay ~= relay and relay.destroyed)
				nextRelay.Changed:Fire(nextRelay)
				relay.Changed:Fire(nil)
				observer.beginChangeJournal("new-host", { "ReplicatedStorage" })
				nextRelay.Changed:Fire(localScript)
				assert(#observer.drainChangeJournal("new-host") == 1, "host restart lost its first native attribute event")
			elseif mode == "persistent" then
				observer.getState({ services = { "ReplicatedStorage" } })
				relay.Changed:Fire(nil)
				assert(localScript.AttributeChanged:Count() == 1, "cached proof lost persistent observation")
			else observer.getState({ stop = true }) end
			assert(not observer.getState({ start = false, verifyPushProof = proof }).pushProofMatches,
				"cached equality survived an outside edit or observation loss")
			observer.stop()
			assert(relay.destroyed and relay.Changed:Count() == 0 and services.RunService.Heartbeat:Count() == 0)
			assert(services.CollectionService.TagAdded:Count() == 0 and services.CollectionService.TagRemoved:Count() == 0)
			assert(tagAdded:Count() == 0 and tagRemoved:Count() == 0, "cached tag memberships leaked listeners")
			assert(documentChanged:Count() == 0, "cached document observation leaked listeners")
		end
	end
	dependencies.BridgeScriptDocuments.capture = savedCapture
	table.clear(collection)
	for key, value in pairs(savedCollection) do collection[key] = value end
	testGame.ItemChanged = savedItemChanged
end
print("Verified push cache: 240 edit/document/tag/membership/expiry/stop/restart/persistent transitions invalidate equality and release listeners")

do
	local savedItemChanged = testGame.ItemChanged
	local savedCapture = dependencies.BridgeScriptDocuments.capture
	for _, mode in { "stable", "terrain", "property", "document-aba", "unarm", "unavailable" } do
		for _ = 1, 20 do
			testGame.ItemChanged = signal()
			local changed = signal()
			local generation, fault = 0, false
			local proofConfig = table.clone(trackerConfig)
			proofConfig.exportProofGeneration = function() return generation end
			proofConfig.observeProofDocuments = function(callback) return { changed:Connect(callback) } end
			dependencies.BridgeScriptDocuments.capture = function() return { { instance = localScript, source = "same" } } end
			local relay
			proofConfig.captureTerrainProof = function()
				if fault then
					if mode == "terrain" then return "different voxels"
					elseif mode == "property" then generation += 1
					elseif mode == "document-aba" then changed:Fire() changed:Fire()
					elseif mode == "unarm" then relay.Changed:Fire(nil)
					elseif mode == "unavailable" then error("DataModel closed during serialization") end
				end
				return "native terrain digest"
			end
			local observer = trackerModule.create(proofConfig, { Workspace = true, ReplicatedStorage = true })
			observer.getState({ services = { "Workspace", "ReplicatedStorage" }, trackingGuardId = "terrain-proof",
				nativeAttributeRelay = true, deferNativeTracking = true })
			relay = createdRelays[#createdRelays]
			relay.Changed:Fire(relay)
			local proof = assert(observer.capturePushProof())
			observer.getState({ start = false, releaseTrackingGuardId = "terrain-proof", retainPushProof = proof })
			fault = true
			assert(observer.verifyPushProof(proof) == (mode == "stable"), "asynchronous Terrain proof accepted changed state")
			observer.stop()
			assert(changed:Count() == 0 and relay.destroyed and services.RunService.Heartbeat:Count() == 0)
		end
	end
	dependencies.BridgeScriptDocuments.capture = savedCapture
	testGame.ItemChanged = savedItemChanged
end
print("Terrain proof: 120 voxel/property/document-ABA/observation-loss/serialization-failure cases preserve the asynchronous fence")


do
	local savedItemChanged, savedCamera = testGame.ItemChanged, workspace.CurrentCamera
	local savedCapture = dependencies.BridgeScriptDocuments.capture
	for _, mode in { "stable", "attribute", "camera-attribute", "camera-replaced", "property", "terrain",
		"document-aba", "tag", "stop", "expiry", "new-lease", "code-only" } do
		testGame.ItemChanged = signal()
		workspace.CurrentCamera = camera
		capturedDelays = {}
		local documentsChanged = signal()
		local generation, digest, fault = 0, "voxels", false
		local config = table.clone(trackerConfig)
		config.exportProofGeneration = function() return generation end
		config.observeProofDocuments = function(callback) return { documentsChanged:Connect(callback) } end
		config.captureTerrainProof = function()
			if fault and mode == "document-aba" then documentsChanged:Fire() documentsChanged:Fire() end
			return digest
		end
		dependencies.BridgeScriptDocuments.capture = function() return { { instance = localScript, source = "same" } } end
		local observer = trackerModule.create(config, { ReplicatedStorage = true, Workspace = true })
		observer.getState({ services = { "ReplicatedStorage", "Workspace" }, trackingGuardId = "local-proof", captureLocalPushProof = true })
		local proof = assert(observer.capturePushProof())
		local released = observer.getState({ start = false, releaseTrackingGuardId = "local-proof", retainPushProof = proof })
		assert(released.retainedPushProof == proof and released.tracking)
		assert(moving.AttributeChanged:Count() == 1, "local cache duplicated its existing attribute listener")
		assert(observer.verifyPushProof(proof))
		fault = true
		if mode == "attribute" then moving.AttributeChanged:Fire("Probe")
		elseif mode == "camera-attribute" then camera.AttributeChanged:Fire("Probe")
		elseif mode == "camera-replaced" then workspace.CurrentCamera = nil
		elseif mode == "property" then generation += 1
		elseif mode == "terrain" then digest = "changed voxels"
		elseif mode == "tag" then services.CollectionService.TagAdded:Fire("new")
		elseif mode == "stop" then observer.getState({ stop = true })
		elseif mode == "expiry" then
			local count = 0
			for _, timer in capturedDelays do
				if timer.seconds == 60 then timer.callback() count += 1 end
			end
			assert(count == 1 and moving.AttributeChanged:Count() == 0 and camera.AttributeChanged:Count() == 0)
		elseif mode == "new-lease" then observer.getState({ services = { "ReplicatedStorage" }, trackingGuardId = "new" })
		elseif mode == "code-only" then observer.setOptions({ onlyCodeMode = true }) end
		assert(observer.verifyPushProof(proof) == (mode == "stable"), "local push proof survived " .. mode)
		observer.stop()
		assert(moving.AttributeChanged:Count() == 0 and camera.AttributeChanged:Count() == 0 and documentsChanged:Count() == 0)
		assert(services.CollectionService.TagAdded:Count() == 0 and services.CollectionService.TagRemoved:Count() == 0)
	end
	capturedDelays = nil
	dependencies.BridgeScriptDocuments.capture = savedCapture
	testGame.ItemChanged, workspace.CurrentCamera = savedItemChanged, savedCamera
end
print("Local push proof: existing observers, raw edits, camera, asynchronous Terrain fence, expiry and lease transitions verified")
