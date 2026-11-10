local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.luau")
local function slice(first, after, from)
	local start = assert(source:find(first, from or 1, true))
	return source:sub(start, assert(source:find(after, start, true)) - 1), start
end
local function folder(name, parent)
	local instance = roblox.Instance.new("Folder")
	instance.Name, instance.Parent = name, parent
	return instance
end
local place = folder("Place")
local service = folder("ReplicatedStorage", place)
local otherService = folder("ServerStorage", place)
local budgetChecks = 0
local denseArrayLength = luau.load(slice("local function denseArrayLength(", "local PATH_SEPARATOR")
	.. "\nreturn denseArrayLength")()
local identity = luau.load(slice("local function settingsIdText(", "local function getStateForService(")
	.. "\nreturn { validate = validatedImportIdentities, prepare = prepareImportedIdentities, remember = rememberImportedIdentities, captured = capturedSettingsId }",
	{ environment = {
		denseArrayLength = denseArrayLength,
		EncodingService = { Base64Decode = function(_, value) return value end },
		BridgeParallel = { newWorkBudget = function() return function() budgetChecks += 1 end end },
		BridgeIdentity = { liveInstance = function(value)
			return if value ~= nil and value.Parent ~= nil and value:IsDescendantOf(place) then value else nil
		end },
		game = { GetService = function(_, name) return if name == "ReplicatedStorage" then service else otherService end },
	} })()

-- The daemon's wire schema: parent row, root/sibling ordinal, name, class, id.
-- EncodingService is an identity mock so these tests can mutate the raw rows.
local function metadata(rows)
	local strings, indices = {}, {}
	local nodes = buffer.create(#rows * 20)
	for index, row in ipairs(rows) do
		for field, value in ipairs(row) do
			if field > 2 then
				local slot = indices[value]
				if slot == nil then slot = #strings; indices[value] = slot; strings[#strings + 1] = value end
				value = slot
			end
			buffer.writeu32(nodes, (index - 1) * 20 + (field - 1) * 4, value)
		end
	end
	return { strings = strings, nodes = buffer.tostring(nodes) }
end
local rows = {
	{ 0, 1, "TreeRoot", "Folder", "editor:6c72" },
	{ 1, 1, "TreeChild", "Folder", "editor:6c73" },
	{ 2, 1, "Leaf", "Folder", "editor:6c74" },
	{ 1, 2, "TreeChild", "Part", "editor:6c75" },
}
local function group()
	return {
		serviceName = "ReplicatedStorage", service = service, target = service, targetPath = { "ReplicatedStorage" },
		rootPaths = { { pathSegments = { "ReplicatedStorage", "TreeRoot" }, pathOrdinals = { 1, 1 } } },
		rootSettingsIds = { "editor:6c72" }, instanceIdentities = identity.validate(metadata(rows), 1),
		incoming = {}, incomingByPayloadIndex = {}, incomingRootsByPath = {}, count = 1,
		payloadRootName = "payload", additive = true, retainedRoots = {}, packageRoots = {},
	}
end
local function tree(parent, name)
	local root = folder(name or "TreeRoot", parent)
	local child = folder("TreeChild", root)
	local leaf = folder("Leaf", child)
	local sibling = roblox.Instance.new("Part")
	sibling.Name, sibling.Parent = "TreeChild", root
	return root, child, leaf, sibling
end
local _, finishStart = slice("\tfunction api.finishBinaryImport(", "\tfunction api.cancelBinaryImport(")
local deserializeGroups = luau.load(
	"return function(session, wrappedRootsByName, outgoingByGroup, ctx)\nlocal prepared, detachedRoots, operation = {}, {}, nil\n"
	.. slice("\t\t\tlocal payloadVerifiedServices = {}", '\t\t\tsetTrackedOperationPhase(operation, "prepareRetention")', finishStart)
	.. "\nreturn prepared\nend", { environment = {
		setTrackedOperationPhase = function() end,
		setParentForSync = function(instance, parent) instance.Parent = parent end,
	} })()
local function key(segments, ordinals) return table.concat(segments, "/") .. table.concat(ordinals, ",") end

for _, path in { "deserialize", "native" } do
	local ctx, prepared = {}, group()
	local root, child, leaf, sibling
	if path == "deserialize" then
		local payload = folder("payload")
		root, child, leaf, sibling = tree(payload, "__ReniumImportRoot_1")
		prepared = deserializeGroups({ groups = { prepared } }, { payload = payload }, { {} }, ctx)[1]
		identity.prepare(prepared)
		root, child, leaf, sibling = table.unpack(prepared.importedSettingsInstances)
		identity.remember({ prepared }, ctx)
		assert(identity.captured(child, ctx) == nil, "Staged descendants must not be reported as live")
		root.Parent = service
		identity.remember({ prepared }, ctx)
	else
		root, child, leaf, sibling = tree(service)
		-- Lune creates new userdata wrappers in GetChildren; Studio returns
		-- the same Instance keys used by its native creation receipt.
		prepared.target = { GetChildren = function() return { root } end,
			GetFullName = function() return service:GetFullName() end }
		-- Run the shipped native completion, including root ordering, identity
		-- preparation and registration before subsequent property writes.
		ctx.finishNativeImportObservations = function() return { ReplicatedStorage = { [root] = prepared.target } } end
		local complete = luau.load(slice("\tlocal function finishNativeServiceImport(", "\tfunction api.finishBinaryImport(")
			.. "\nreturn finishNativeServiceImport", { environment = {
				ctx = ctx, pathCacheKey = key,
				prepareImportedIdentities = identity.prepare, rememberImportedIdentities = identity.remember,
				beginHistoryRecording = function() return "recording" end,
				runWithStudioChangeSuppression = function(_, callback) callback() end,
				ReferenceOverlay = { apply = function() end },
			} })()
		local native = { readerArmed = true, invocationAuthorized = true, receivedCreated = 4, expectedCreated = 4,
			createdById = {}, boundPaths = {}, started = os.clock() }
		local result = complete({ nativeHeld = native, totalBytes = 100 }, {
			assertOwnership = function() end, nativeUndo = { prepared = { prepared }, replacements = {} },
		}, { nativePhase = "complete", nativeStatus = 4, nativeState = 14, nativeCreated = 4 })
		assert(result.nativeInserted)
		root, child, leaf, sibling = table.unpack(prepared.importedSettingsInstances)
	end
	assert(identity.captured(child, ctx) == "editor:6c73" and identity.captured(leaf, ctx) == "editor:6c74",
		`{path}: child={identity.captured(child, ctx)}, leaf={identity.captured(leaf, ctx)}, prepared={#(prepared.importedSettingsInstances or {})}`)
	assert(identity.captured(sibling, ctx) == "editor:6c75", "Duplicate names retain distinct ordinals")
	local holder = roblox.Instance.new("ObjectValue")
	holder.Parent, holder.Value = otherService, child
	child.Name = "ChildMoved"
	local newcomer = folder("TreeChild", root)
	assert(holder.Value == child and identity.captured(child, ctx) == "editor:6c73")
	assert(identity.captured(newcomer, ctx) == nil, "A newcomer cannot inherit the renamed child's identity")
	child.Parent = folder("MovedUnder", root)
	assert(identity.captured(child, ctx) == "editor:6c73")
	child.Parent = otherService
	assert(identity.captured(child, ctx) == nil, "Store ids remain scoped to their service")
	child.Parent = root
	assert(identity.captured(child, ctx) == "editor:6c73")
	root.Parent = nil
	assert(identity.captured(child, ctx) == nil, "Rolled-back imports leave no live binding")
	root:Destroy(); holder:Destroy()
end

-- Retention keeps existing package/viewport objects in place of staged ones.
-- Register their aliases, including descendants, rather than detached copies.
do
	local prepared = group()
	local duplicate = tree()
	prepared.incomingByPayloadIndex[1] = duplicate
	identity.prepare(prepared)
	local retained = tree(service)
	local liveGroup = group()
	liveGroup.incomingByPayloadIndex[1] = retained
	identity.prepare(liveGroup)
	local aliases = {}
	for index, instance in ipairs(prepared.importedSettingsInstances) do
		aliases[instance] = liveGroup.importedSettingsInstances[index]
	end
	local ctx = {}
	identity.remember({ prepared }, ctx, aliases)
	for index, instance in ipairs(liveGroup.importedSettingsInstances) do
		assert(identity.captured(instance, ctx) == rows[index][5], "Retained objects receive their store identities")
		assert(identity.captured(prepared.importedSettingsInstances[index], ctx) == nil)
	end
	duplicate:Destroy(); retained:Destroy()
end

for _, mutate in {
	function(raw) raw.nodes = raw.nodes:sub(2) end,
	function(raw) raw.strings[1] = false end,
	function(raw) local b = buffer.fromstring(raw.nodes); buffer.writeu32(b, 20, 2); raw.nodes = buffer.tostring(b) end,
	function(raw) local b = buffer.fromstring(raw.nodes); buffer.writeu32(b, 4, 2); raw.nodes = buffer.tostring(b) end,
	function(raw) local b = buffer.fromstring(raw.nodes); buffer.writeu32(b, 16, 9999); raw.nodes = buffer.tostring(b) end,
	function(raw) local b = buffer.fromstring(raw.nodes); buffer.writeu32(b, 36, buffer.readu32(b, 16)); raw.nodes = buffer.tostring(b) end,
} do
	local raw = metadata(rows); mutate(raw)
	assert(not pcall(identity.validate, raw, 1), "Malformed identity metadata must be refused")
end
do
	local prepared = group()
	local root, child = tree()
	prepared.incomingByPayloadIndex[1] = root
	child.Name = "Wrong"
	assert(not pcall(identity.prepare, prepared), "Missing payload identities must not be guessed")
	root:Destroy()
end

-- A place-sized wide import: one sibling scan per parent, a budget check for
-- every metadata row, child and registration; no repeated full-path search.
do
	local count = 20_000
	local largeRows = { { 0, 1, "Wide", "Folder", "editor:root" } }
	local root = folder("Wide", service)
	for index = 1, count do
		folder("Child", root)
		largeRows[#largeRows + 1] = { 1, index, "Child", "Folder", `editor:{index}` }
	end
	budgetChecks = 0
	local prepared = { serviceName = "ReplicatedStorage", incomingByPayloadIndex = { root },
		instanceIdentities = identity.validate(metadata(largeRows), 1) }
	identity.prepare(prepared)
	local ctx = {}
	identity.remember({ prepared }, ctx)
	assert(identity.captured(prepared.importedSettingsInstances[count + 1], ctx) == `editor:{count}`)
	assert(budgetChecks >= count * 4, "Large imports must yield through the shared work budget at item boundaries")
	root:Destroy()
end
place:Destroy()
print("Imported subtree identities: native and deserialize paths, nested/duplicate nodes, rename/newcomer, scope, refusal and 20k-node work budget")
