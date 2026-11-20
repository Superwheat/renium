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
local function budget()
	budgetChecks += 1
	return false
end
local parallel = { newWorkBudget = function() return budget end, newRequestBudget = function() return budget end }
local identity = luau.load(slice("local function settingsIdText(", "local function getStateForService(")
	.. "\nreturn { validate = validatedImportIdentities, prepare = prepareImportedIdentities, remember = rememberImportedIdentities, captured = capturedSettingsId }",
	{ environment = {
		BridgeConnection = require("../BridgeConnection.module"),
		denseArrayLength = denseArrayLength,
		EncodingService = { Base64Decode = function(_, value) return value end },
		BridgeParallel = parallel,
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
local function partName(index)
	return string.format("Part%05d", index) .. string.rep("x", 91)
end
local function group(extraCount)
	local importedRows = table.clone(rows)
	for index = 1, extraCount or 0 do
		importedRows[#importedRows + 1] = { 1, 1, partName(index), "Part", `editor:wide{index}` }
	end
	return {
		serviceName = "ReplicatedStorage", service = service, target = service, targetPath = { "ReplicatedStorage" },
		rootPaths = { { pathSegments = { "ReplicatedStorage", "TreeRoot" }, pathOrdinals = { 1, 1 } } },
		rootSettingsIds = { "editor:6c72" }, instanceIdentities = identity.validate(metadata(importedRows), 1, budget),
		incoming = {}, incomingByPayloadIndex = {}, incomingRootsByPath = {}, count = 1,
		payloadRootName = "payload", additive = true, retainedRoots = {}, packageRoots = {},
	}
end
local function tree(parent, name, extraCount)
	local root = folder(name or "TreeRoot", parent)
	local child = folder("TreeChild", root)
	local leaf = folder("Leaf", child)
	local sibling = roblox.Instance.new("Part")
	sibling.Name, sibling.Parent = "TreeChild", root
	for index = 1, extraCount or 0 do
		local part = roblox.Instance.new("Part")
		part.Name, part.Parent = partName(index), root
	end
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
-- The shipped native completion: root ordering, identity preparation and
-- registration, and the export list it leaves for the service.
local function nativeCompletion(ctx, target)
	return luau.load(slice("\tlocal function finishNativeServiceImport(", "\tfunction api.finishBinaryImport(")
		.. "\nreturn finishNativeServiceImport", { environment = {
			BridgeConnection = require("../BridgeConnection.module"), BridgeParallel = parallel,
			ctx = ctx, pathCacheKey = key,
			prepareImportedIdentities = identity.prepare, rememberImportedIdentities = identity.remember,
			beginHistoryRecording = function() return "recording" end,
			runWithStudioChangeSuppression = function(_, callback) callback() end,
			ReferenceOverlay = { apply = function() end },
			game = { GetService = function() return target end },
		} })()
end
local function completeNative(prepared, created)
	return nativeCompletion(prepared.ctx, prepared.target)({ nativeHeld = { readerArmed = true, invocationAuthorized = true,
		receivedCreated = created, expectedCreated = created, createdById = {}, boundPaths = {}, started = os.clock() },
		totalBytes = 100 }, { assertOwnership = function() end, nativeUndo = { prepared = { prepared }, replacements = {} } },
		{ nativePhase = "complete", nativeStatus = 4, nativeState = 14, nativeCreated = created })
end

-- Native completion runs once after every 512-instance reader batch has
-- finished. Both transports must register the complete tree at that point.
for _, case in { { "deserialize", 0 }, { "native", 0 }, { "deserialize", 6000 }, { "native", 6000 } } do
	local path, count = case[1], case[2]
	local ctx, prepared = {}, group(count)
	local root, child, leaf, sibling
	if path == "deserialize" then
		local payload = folder("payload")
		root, child, leaf, sibling = tree(payload, "__ReniumImportRoot_1", count)
		prepared = deserializeGroups({ groups = { prepared } }, { payload = payload }, { {} }, ctx)[1]
		identity.prepare(prepared, budget)
		root, child, leaf, sibling = table.unpack(prepared.importedSettingsInstances, 1, 4)
		identity.remember({ prepared }, ctx, nil, budget)
		assert(identity.captured(child, ctx) == nil, "Staged descendants must not be reported as live")
		root.Parent = service
		identity.remember({ prepared }, ctx, nil, budget)
	else
		root, child, leaf, sibling = tree(service, nil, count)
		-- Lune creates new userdata wrappers in GetChildren; Studio returns
		-- the same Instance keys used by its native creation receipt.
		prepared.target = { GetChildren = function() return { root } end,
			GetFullName = function() return service:GetFullName() end }
		-- Run the shipped native completion, including root ordering, identity
		-- preparation and registration before subsequent property writes.
		ctx.finishNativeImportObservations = function() return { ReplicatedStorage = { [root] = prepared.target } } end
		prepared.ctx = ctx
		assert(completeNative(prepared, count + 4).nativeInserted)
		root, child, leaf, sibling = table.unpack(prepared.importedSettingsInstances, 1, 4)
	end
	assert(#prepared.importedSettingsInstances == count + 4)
	for index = 1, count do
		assert(identity.captured(prepared.importedSettingsInstances[index + 4], ctx) == `editor:wide{index}`,
			`{path}: every imported descendant must keep its store id across batches`)
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
	identity.prepare(prepared, budget)
	local retained = tree(service)
	local liveGroup = group()
	liveGroup.incomingByPayloadIndex[1] = retained
	identity.prepare(liveGroup, budget)
	local aliases = {}
	for index, instance in ipairs(prepared.importedSettingsInstances) do
		aliases[instance] = liveGroup.importedSettingsInstances[index]
	end
	local ctx = {}
	identity.remember({ prepared }, ctx, aliases, budget)
	for index, instance in ipairs(liveGroup.importedSettingsInstances) do
		assert(identity.captured(instance, ctx) == rows[index][5], "Retained objects receive their store identities")
		assert(identity.captured(prepared.importedSettingsInstances[index], ctx) == nil)
	end
	duplicate:Destroy(); retained:Destroy()
end

-- Another request may replace the cached lookup and the match tables while
-- registration yields: the rows bound after the yield land in the new ones.
do
	local prepared = group()
	prepared.incomingByPayloadIndex[1] = tree(service)
	identity.prepare(prepared, budget)
	local bound = prepared.importedSettingsInstances
	local old, fresh = { lookup = {} }, { lookup = {} }
	local ctx = { settingsIdLookupByService = { ReplicatedStorage = old } }
	local checks = 0
	identity.remember({ prepared }, ctx, nil, function()
		checks += 1
		-- The first check is the root id's; the fourth comes before row 3.
		if checks ~= 4 then return false end
		ctx.settingsIdLookupByService = { ReplicatedStorage = fresh }
		ctx.matchedSettingsInstancesByService = { ReplicatedStorage = {} }
		return true
	end)
	local matches = ctx.matchedSettingsInstancesByService.ReplicatedStorage
	for index = 3, 4 do
		local id = rows[index][5]
		assert(fresh.lookup[id] == bound[index] and old.lookup[id] == nil,
			`row {index} went into the lookup that was replaced during the yield`)
		assert(matches[id] == bound[index], `row {index} went into the match table that was replaced during the yield`)
	end
	assert(old.lookup[rows[2][5]] == bound[2] and fresh.lookup[rows[2][5]] == nil, "rows before the yield use the tables of their time")
	bound[1]:Destroy()
end

do
	local profile = require("../BridgeConnection.module")
	profile.profileStart(os.clock())
	local prepared = group()
	local root = tree(service)
	prepared.incomingByPayloadIndex[1] = root
	identity.prepare(prepared, budget)
	identity.remember({ prepared }, {}, nil, budget)
	local labels = {}
	for _, span in ipairs(profile.profileFinish().spans) do
		labels[span[1]] = span
		assert(span[2] >= 0 and span[3] >= span[2])
	end
	assert(labels["import identity metadata: validation work"])
	assert(labels["import live identities: validation work"])
	assert(labels["import identity registration: binding work"])
	local counts = labels["import live identity traversal"][4]
	assert(counts.instances == 4 and counts.calls == 2, "Count every validated row and actual GetChildren call")
	root:Destroy()
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
	assert(not pcall(identity.validate, raw, 1, budget), "Malformed identity metadata must be refused")
end
-- The strings as one text joined by NUL: the same strings, the same checks.
do
	local raw = metadata(rows)
	local function joined()
		return { stringText = table.concat(raw.strings, "\0"), stringCount = #raw.strings, nodes = raw.nodes }
	end
	local validated = identity.validate(joined(), 1, budget)
	assert(#validated.strings == #raw.strings, "Joined strings must split into as many strings")
	for index, text in ipairs(raw.strings) do
		assert(validated.strings[index] == text, "Joined strings must split into the same strings")
	end
	for _, mutate in {
		function(value) value.stringCount += 1 end,
		function(value) value.stringCount = -1 end,
		function(value) value.stringCount = nil end,
		function(value) local b = buffer.fromstring(value.nodes); buffer.writeu32(b, 36, buffer.readu32(b, 16)); value.nodes = buffer.tostring(b) end,
	} do
		local value = joined(); mutate(value)
		assert(not pcall(identity.validate, value, 1, budget), "Joined identity strings must be refused when they do not match")
	end
end
do
	local prepared = group()
	local root, child, leaf = tree()
	prepared.incomingByPayloadIndex[1] = root
	child.Name = "Wrong"
	assert(not pcall(identity.prepare, prepared, budget), "Missing payload identities must not be guessed")
	child.Name = "TreeChild"
	local oversized = table.clone(rows)
	oversized[3] = { 2, 1, string.rep("x", 129), "Folder", "editor:6c74" }
	prepared.instanceIdentities = identity.validate(metadata(oversized), 1, budget)
	-- Lune does not enforce Studio's Name limit; model the engine's result.
	leaf.Name = string.rep("x", 100)
	local accepted, refusal = pcall(identity.prepare, prepared, budget)
	assert(not accepted, "Truncated names must not be accepted as the requested identity")
	assert(string.find(tostring(refusal), "129 bytes and Studio keeps 100", 1, true), tostring(refusal))
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
		instanceIdentities = identity.validate(metadata(largeRows), 1, budget) }
	identity.prepare(prepared, budget)
	local ctx = {}
	identity.remember({ prepared }, ctx, nil, budget)
	assert(identity.captured(prepared.importedSettingsInstances[count + 1], ctx) == `editor:{count}`)
	assert(budgetChecks >= count * 4, "Large imports must yield through the shared work budget at item boundaries")
	root:Destroy()
end
place:Destroy()

-- Rows bound to the receipted additions by parent and name. Studio hands out one
-- userdata per Instance, which Lune does not, so these objects stand in for it.
do
	local engineTypeof = typeof
	local Mock = {}
	Mock.__index = Mock
	local function mock(className, name, parent)
		local object = setmetatable({ ClassName = className, Name = name, Parent = parent, children = {}, reads = 0 }, Mock)
		if parent then
			table.insert(parent.children, object)
		end
		return object
	end
	function Mock:GetChildren()
		self.reads += 1
		return table.clone(self.children)
	end
	function Mock:GetDescendants()
		local list = {}
		for _, child in ipairs(self.children) do
			list[#list + 1] = child
			local below = child:GetDescendants()
			table.move(below, 1, #below, #list + 1, list)
		end
		return list
	end
	function Mock:IsDescendantOf(ancestor)
		local above = self.Parent
		while above ~= nil and above ~= ancestor do
			above = above.Parent
		end
		return above ~= nil
	end
	function Mock:GetFullName()
		return self.Name
	end
	local mockPlace = mock("DataModel", "Place")
	local mockService = mock("ReplicatedStorage", "ReplicatedStorage", mockPlace)
	local mockGame = { GetService = function() return mockService end }
	local mocked = luau.load(slice("local function settingsIdText(", "local function getStateForService(")
		.. "\nreturn { prepare = prepareImportedIdentities, remember = rememberImportedIdentities }",
		{ environment = {
			BridgeConnection = require("../BridgeConnection.module"),
			BridgeParallel = parallel,
			typeof = function(value) return if getmetatable(value) == Mock then "Instance" else engineTypeof(value) end,
			BridgeIdentity = { liveInstance = function(value)
				return if value ~= nil and value.Parent ~= nil and value:IsDescendantOf(mockPlace) then value else nil
			end },
			game = mockGame,
		} })()
	local rows = {
		{ 0, 1, "Positional", "Folder", "editor:p0" },
		{ 1, 1, "A", "Folder", "editor:pa" },
		{ 2, 1, "A1", "Folder", "editor:pa1" },
		{ 1, 1, "B", "Folder", "editor:pb" },
		{ 4, 1, "X", "Folder", "editor:px1" },
		{ 5, 1, "Y", "Folder", "editor:py1" },
		{ 4, 2, "X", "Folder", "editor:px2" },
		{ 7, 1, "Y", "Folder", "editor:py2" },
		{ 1, 1, "C", "Part", "editor:pc" },
		{ 4, 1, "Z", "Folder", "editor:pz" },
	}
	local function build(lastClass)
		local root = mock("Folder", "Positional", mockService)
		local a = mock("Folder", "A", root)
		local a1 = mock("Folder", "A1", a)
		local b = mock("Folder", "B", root)
		local x1 = mock("Folder", "X", b)
		local y1 = mock("Folder", "Y", x1)
		local x2 = mock("Folder", "X", b)
		local y2 = mock("Folder", "Y", x2)
		local c = mock(lastClass or "Part", "C", root)
		return { root, a, a1, b, x1, y1, x2, y2, c, mock("Folder", "Z", b) }
	end
	-- The shipped native completion over one built tree and its receipted
	-- additions, each under the parent it has.
	local function run(instances, added, identityRows)
		local parents = {}
		for _, instance in ipairs(added) do
			parents[instance] = instance.Parent
		end
		local ctx = {
			finishNativeImportObservations = function()
				return { ReplicatedStorage = parents }
			end,
		}
		local complete = luau.load(slice("\tlocal function finishNativeServiceImport(", "\tfunction api.finishBinaryImport(")
			.. "\nreturn finishNativeServiceImport", { environment = {
				BridgeConnection = require("../BridgeConnection.module"), BridgeParallel = parallel, pathCacheKey = key,
				prepareImportedIdentities = mocked.prepare, rememberImportedIdentities = mocked.remember,
				beginHistoryRecording = function() return "recording" end,
				runWithStudioChangeSuppression = function(_, callback) callback() end,
				ReferenceOverlay = { apply = function() end }, game = mockGame, ctx = ctx,
			} })()
		local prepared = { serviceName = "ReplicatedStorage", target = mockService, targetPath = { "ReplicatedStorage" },
			rootPaths = { { pathSegments = { "ReplicatedStorage", "Positional" }, pathOrdinals = { 1, 1 } } },
			rootSettingsIds = { "editor:p0" }, instanceIdentities = identity.validate(metadata(identityRows or rows), 1, budget),
			incoming = {}, incomingByPayloadIndex = {}, incomingRootsByPath = {} }
		local profile = require("../BridgeConnection.module")
		profile.profileStart(os.clock())
		local ok, failure = pcall(complete, { nativeHeld = { readerArmed = true, invocationAuthorized = true,
			receivedCreated = #instances, expectedCreated = #instances, createdById = {}, boundPaths = {},
			started = os.clock() }, totalBytes = 100 }, { assertOwnership = function() end,
			nativeUndo = { prepared = { prepared }, replacements = {} } },
			{ nativePhase = "complete", nativeStatus = 4, nativeState = 14, nativeCreated = #instances })
		local counts
		for _, span in ipairs(profile.profileFinish().spans) do
			if span[1] == "import live identity traversal" then
				counts = span[4]
			end
		end
		return ok, failure, prepared, ctx, counts
	end
	local function detach(instances)
		instances[1].Parent = nil
		table.remove(mockService.children, table.find(mockService.children, instances[1]))
	end
	local function assertBound(prepared, ctx, instances, label, identityRows)
		for index, instance in ipairs(instances) do
			assert(prepared.importedSettingsInstances[index] == instance, `{label}: row {index} bound another object`)
			assert(ctx.matchedSettingsIdByInstance[instance] == (identityRows or rows)[index][5],
				`{label}: row {index} must carry its own id`)
		end
	end
	local function refusal(ok, failure)
		return not ok and string.find(tostring(failure), "does not match the payload", 1, true) ~= nil
	end

	-- Every row whose name is unique under its parent takes its addition,
	-- Z beside the two X folders included; only those X folders' parent is read.
	local instances = build()
	local ok, failure, prepared, ctx, counts = run(instances, instances)
	assert(ok, failure)
	assertBound(prepared, ctx, instances, "additions by parent and name")
	assert(counts.arrived == 7 and counts.calls == 1 and instances[4].reads == 1 and instances[2].reads == 0,
		`only the parent of the two X folders is read: arrived {counts.arrived}, calls {counts.calls}`)
	detach(instances)

	-- An addition no row names is left alone; the other rows still take theirs.
	instances = build()
	local extra = mock("Folder", "Extra", instances[2])
	local added = table.clone(instances)
	added[#added + 1] = extra
	ok, failure, prepared, ctx, counts = run(instances, added)
	assert(ok, failure)
	assertBound(prepared, ctx, instances, "unnamed addition")
	assert(counts.arrived == 7, `an unnamed addition must not cost the other rows theirs ({counts.arrived})`)
	detach(instances)

	-- Two additions of one name under a parent whose rows name it once: the
	-- row is found among the parent's children, by Studio's child order.
	instances = build()
	local twin = mock("Folder", "A1", instances[2])
	added = table.clone(instances)
	added[#added + 1] = twin
	ok, failure, prepared, ctx, counts = run(instances, added)
	assert(ok, failure)
	assertBound(prepared, ctx, instances, "shared addition name")
	assert(instances[2].reads == 1, "a name two additions share must be read from the parent's children")
	detach(instances)

	-- Every mismatch is still refused: two rows of one name both first under
	-- their parent, a class, an ordinal no sibling has, and a name.
	local twice = table.clone(rows)
	twice[7] = { 4, 1, "X", "Folder", "editor:px2" }
	instances = build()
	assert(refusal(run(instances, instances, twice)), "Two first rows of one name under a parent must be refused")
	detach(instances)
	instances = build()
	assert(refusal(run(instances, { instances[1], instances[2], instances[3], instances[4], instances[5], instances[6],
		instances[8], instances[9], instances[10] }, twice)), "Two first rows of one name must be refused with one addition")
	detach(instances)
	instances = build("Folder")
	assert(refusal(run(instances, instances)), "A class mismatch must be refused")
	detach(instances)
	local wrongOrdinal = table.clone(rows)
	wrongOrdinal[8] = { 7, 2, "Y", "Folder", "editor:py2" }
	instances = build()
	assert(refusal(run(instances, instances, wrongOrdinal)), "An ordinal no sibling has must be refused")
	detach(instances)
	instances = build()
	instances[3].Name = "Renamed"
	assert(refusal(run(instances, instances)), "A name mismatch must be refused")
	detach(instances)
end
-- One budget per request: every group of a begin, a staged finish and a native
-- completion spends the same one, so groups that each stay under a slice
-- still yield once their sum is spent.
do
	local created, spent = 0, {}
	local function counting()
		created += 1
		return function() return false end
	end
	local function spend(budgetArgument)
		spent[#spent + 1] = budgetArgument
	end
	local function oneBudget(label)
		assert(created == 1 and #spent == 2 and spent[1] == spent[2], `{label}: its groups must share one budget`)
		created, spent = 0, {}
	end
	local validateGroups = luau.load(slice("\tlocal function validatedNativeImportGroups(", "\tlocal function prepareNativeServiceTargets(")
		.. "\nreturn validatedNativeImportGroups", { environment = {
			denseArrayLength = denseArrayLength, BridgeParallel = { newRequestBudget = counting },
			validatedNativeImportGroup = function(_, _, _, budgetArgument) spend(budgetArgument) return {} end,
		} })()
	validateGroups({ {}, {} })
	oneBudget("begin")
	local stagedFinish = luau.load("return function(prepared, operation)\n"
		.. slice('\t\t\tsetTrackedOperationPhase(operation, "prepareRetention")', "\t\t\tassertImportActive()") .. "\nend",
		{ environment = {
			setTrackedOperationPhase = function() end, BridgeParallel = { newRequestBudget = counting },
			prepareImportedIdentities = function(_, budgetArgument) spend(budgetArgument) end,
		} })()
	stagedFinish({ {}, {} })
	oneBudget("staged finish")
	local completion = luau.load(slice("\tlocal function finishNativeServiceImport(", "\tfunction api.finishBinaryImport(")
		.. "\nreturn finishNativeServiceImport", { environment = {
			BridgeConnection = require("../BridgeConnection.module"), BridgeParallel = { newRequestBudget = counting },
			pathCacheKey = key, ctx = { finishNativeImportObservations = function() return {} end },
			prepareImportedIdentities = function(_, budgetArgument) spend(budgetArgument) end,
			rememberImportedIdentities = function(_, _, _, budgetArgument) spend(budgetArgument) end,
			beginHistoryRecording = function() return "recording" end,
			runWithStudioChangeSuppression = function(_, callback) callback() end,
			ReferenceOverlay = { apply = function() end }, game = { GetService = function() return service end },
		} })()
	local target = { GetChildren = function() return {} end }
	local function emptyGroup()
		return { serviceName = "ReplicatedStorage", target = target, rootPaths = {}, incoming = {},
			incomingByPayloadIndex = {}, incomingRootsByPath = {} }
	end
	completion({ nativeHeld = { readerArmed = true, invocationAuthorized = true, receivedCreated = 0, expectedCreated = 0,
		createdById = {}, boundPaths = {}, started = os.clock() }, totalBytes = 100 }, { assertOwnership = function() end,
		nativeUndo = { prepared = { emptyGroup(), emptyGroup() }, replacements = {} } },
		{ nativePhase = "complete", nativeStatus = 4, nativeState = 14, nativeCreated = 0 })
	assert(created == 1 and #spent == 3 and spent[1] == spent[2] and spent[2] == spent[3],
		"native completion: its groups and the registration must share one budget")
end

print("Imported subtree identities: native and deserialize 6k-node trees, nested/duplicate nodes, rename/newcomer, scope, truncation refusal and 20k-node work budget")
print("Live identities: additions bound by parent and name, one read for same-named siblings or a shared addition name, and every refusal")
