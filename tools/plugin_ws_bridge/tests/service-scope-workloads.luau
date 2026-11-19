local fs = require("@lune/fs")
local luau = require("@lune/luau")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.luau")
local function extract(first, after, environment, returned)
	local start = assert(source:find(first, 1, true))
	local finish = assert(source:find(after, start, true))
	return luau.load(source:sub(start, finish - 1) .. "\nreturn " .. returned, { environment = environment })()
end
local services = {}
local environment = { game = { GetService = function(_, name) return services[name] end } }
local policy = extract("local ENGINE_MANAGED_CONTAINERS =", "local INSTANCE_CHANGE_MODES =", environment,
	"{ classes = ENGINE_MANAGED_CONTAINERS, predicate = isEngineManagedContainerEntry, instance = isEngineManagedContainerInstance }")
local function node(className)
	return {
		ClassName = className, Name = className, children = {},
		GetChildren = function(self) return self.children end,
		IsA = function(self, class) return self.ClassName == class end,
		FindFirstChildOfClass = function(self, class)
			for _, child in self.children do if child.ClassName == class then return child end end
			return nil
		end,
	}
end
for serviceName, classes in policy.classes do
	local service = node(serviceName)
	services[serviceName] = service
	for _, className in classes do
		local container = node(className)
		container.Parent = service
		container.children = { node("ObjectValue") }
		service.children[#service.children + 1] = container
		assert(policy.predicate(serviceName, { className = className, pathSegments = { serviceName, className } }))
		assert(not policy.predicate("ReplicatedStorage", { className = className, pathSegments = { "ReplicatedStorage", className } }))
		assert(not policy.predicate(serviceName, { className = "Folder", pathSegments = { serviceName, className } }))
		assert(policy.instance(serviceName, container))
		assert(not policy.instance("ReplicatedStorage", container))
	end
end
local layout = extract("local function addSnapshotMetadataTarget(", "local function serializeSnapshotRoots(", {
	ENGINE_MANAGED_CONTAINERS = policy.classes,
	game = { GetService = function(_, name) return services[name] end }, Workspace = services.Workspace,
	includeManagedInstance = function() return true end, containsPackageLink = function() return false end,
}, "mutationSnapshotLayout")
for serviceName, service in services do
	local groups, roots, metadata = layout({ serviceName }, {})
	assert(#groups == #service.children + 1 and #roots == #service.children)
	assert(#metadata == #service.children + 1)
	assert(groups[#groups].count == 0, "Engine containers cannot be serialized as replaceable roots")
	for index, container in service.children do
		assert(groups[index].target == container and groups[index].count == 1)
		assert(groups[#groups].preserved[container])
		assert(roots[index] == container.children[1], "Contents retain snapshot coverage")
	end
	local selected = service.children[1]
	local scoped, scopedRoots = layout({ serviceName }, {}, nil,
		{ [serviceName] = { [selected] = true } }, { [serviceName] = true })
	assert(#scoped == 2 and #scopedRoots == 1 and scopedRoots[1] == selected.children[1])
end
local deleted = {}
local function remove(instance)
	deleted[instance] = true
	instance.Parent = nil
end
local deletionEnvironment = {
	isEngineManagedContainerInstance = policy.instance,
	isProtectedWorkspaceCameraInstance = function() return false end,
	includeManagedInstance = function() return true end,
	shouldKeepInstanceByDesiredEntry = function() return false end,
	BridgeIdentity = { newPathSnapshot = function() return {} end,
		getRefPathParts = function(instance) return { instance.Name }, { 1 } end },
	pathCacheKey = function(parts) return parts[1] end,
	removeInstanceForUndo = remove,
	validatedChangeService = function(change) return change.service, services[change.service] end,
	sortedInstanceEntries = function(change) return change.instances end,
	resolveEntryInstance = function(entry) return entry.instance end,
	rememberEntryResolution = function() end,
}
local prune = extract("local function removeUnknownInstances(", "local function applyInstanceReconcile(",
	deletionEnvironment, "removeUnknownInstances")
local delete = extract("local function applyInstanceDeletes(", "local function applyInstanceChange(",
	deletionEnvironment, "applyInstanceDeletes")
for serviceName, classes in policy.classes do
	for _, className in classes do
		for _, explicit in { false, true } do
			table.clear(deleted)
			local service = services[serviceName]
			local container = node(className)
			container.Parent = service
			local child = node("Folder")
			child.Name, child.Parent = "RemovedContent", container
			local namesake = node("Folder")
			namesake.Name, namesake.Parent = className, service
			service.GetDescendants = function() return { container, child, namesake } end
			local stats = { instanceDeleted = 0, noops = 0 }
			if explicit then
				local entries = {}
				for _, instance in { container, child, namesake } do
					entries[#entries + 1] = { instance = instance, pathSegments = { serviceName, instance.Name } }
				end
				delete({ service = serviceName, instances = entries }, {}, stats, {})
				assert(stats.instancesVerified[serviceName] == 2,
					"Skipped engine anchors must not receive a deletion proof")
			else
				prune(serviceName, service, {}, stats, {}, {}, {}, {})
			end
			assert(container.Parent == service and not deleted[container], "Engine anchor must remain")
			assert(deleted[child] and deleted[namesake] and stats.instanceDeleted == 2,
				"Anchor protection must not protect ordinary contents or same-named folders")
		end
	end
end
print("Service scope: engine-owned containers retained, contents snapshotted, scoped edits remain scoped")

do
	local parent = node("Folder")
	local upsert = extract("local function applyInstanceUpserts(", "local function applyInstanceDeletes(", {
		validatedChangeService = function() return "Workspace", services.Workspace end,
		sortedInstanceEntries = function(change) return change.instances end,
		liveHydrateEnabled = function() return true end,
		syncDesiredEntry = function(entry) return entry.instance end,
		resolveEntryParent = function() return parent end,
	}, "applyInstanceUpserts")
	for _, mismatch in { "none", "class", "name", "parent", "missing" } do
		for _ = 1, 20 do
			local instance = node(if mismatch == "class" then "Part" else "Folder")
			instance.Name = if mismatch == "name" then "Wrong" else "Wanted"
			instance.Parent = if mismatch == "parent" then node("Folder") else parent
			local stats = { instanceCreated = 0, instanceReplaced = 0, propertyUpdated = 0, noops = 0 }
			upsert({ instances = { {
				instance = if mismatch == "missing" then nil else instance,
				className = "Folder", pathSegments = { "Workspace", "Parent", "Wanted" }, previousPathSegments = {},
			} } }, {}, stats, {})
			assert(stats.instancesVerified.Workspace == (if mismatch == "none" then 1 else 0),
				"Structural receipt accepted a missing or misplaced instance")
		end
	end
end
print("Structural receipts: 100 class/name/parent/presence checks reject incomplete upserts; skipped containers remain unverified")

do
	-- The children listed for the previous-place pass are stale after a yield
	-- and belong to that pass alone.
	local outer, seen, yields = {}, {}, 0
	local ctx = { resolveChildIndex = outer }
	ctx.yieldForBudget = function()
		assert(ctx.resolveChildIndex == outer, "A yield must not show the pass's child index to other work")
		yields += 1
		return yields == 3
	end
	local function upsert(resolve)
		return extract("local function applyInstanceUpserts(", "local function applyInstanceDeletes(", {
			validatedChangeService = function() return "Workspace", services.Workspace end,
			sortedInstanceEntries = function(change) return change.instances end,
			liveHydrateEnabled = function() return false end,
			syncDesiredEntry = function() assert(ctx.resolveChildIndex == outer, "The pass kept its child index") end,
			resolveEntryParent = function() return nil end,
			rememberEntryResolution = function() end,
			describeUnresolvedPrevious = function() return "gone" end,
			resolveEntryInstance = resolve,
		}, "applyInstanceUpserts")
	end
	local entries = {}
	for index = 1, 3 do
		entries[index] = { key = index, pathSegments = { "Workspace", "Rail" }, previousPathSegments = { "Workspace", "Rail" } }
	end
	local stats = { instanceCreated = 0, instanceReplaced = 0, propertyUpdated = 0, noops = 0 }
	upsert(function(entry)
		assert(ctx.resolveChildIndex ~= outer, "The pass resolves through its own child index")
		seen[entry.key] = next(ctx.resolveChildIndex) ~= nil
		ctx.resolveChildIndex.listed = true
		return node("Part")
	end)({ instances = entries }, ctx, stats, {})
	assert(not seen[1] and seen[2] and not seen[3], "A yield must drop the children listed before it")
	assert(ctx.resolveChildIndex == outer, "The caller's child index must be back after the pass")
	local ok = pcall(upsert(function() return nil end), { instances = entries }, ctx, stats, {})
	assert(not ok and ctx.resolveChildIndex == outer, "A failed pass must give the caller's child index back")
end
print("Previous-place pass: its child index is dropped at a yield and never outlives the pass")

do
	local Identity = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeIdentity.module.luau"), {
		environment = { game = {} },
	})()
	local cloneArray = extract("local function cloneArray(", "local function denseArrayLength(", {}, "cloneArray")
	local sortedInstanceEntries = extract("local function entryApplyOrder(", "local function syncDesiredEntry(", {
		cloneArray = cloneArray, pathKey = Identity.pathKey, pathCacheKey = Identity.pathCacheKey,
		settingsIdText = function(raw) return if type(raw) == "string" then raw else nil end,
	}, "sortedInstanceEntries")
	local renamed = node("ServerStorage")
	renamed.Name = "Vault"
	local seen = {}
	local function record(name)
		return function(_, serviceName) seen[name] = serviceName end
	end
	local shared = {
		validatedChangeService = function() return "ServerStorage", renamed end,
		sortedInstanceEntries = sortedInstanceEntries,
		resolveEntryInstance = function(_, serviceName) seen.resolve = serviceName return nil end,
		rememberEntryResolution = function() end,
	}
	local function with(extra)
		local environment = table.clone(shared)
		for key, value in extra do environment[key] = value end
		return environment
	end
	local change = { service = "ServerStorage", instances = { {
		className = "Folder", pathSegments = { "ServerStorage", "Pushed" }, pathOrdinals = { 1, 1 },
	} } }
	local function stats()
		return { instanceCreated = 0, instanceReplaced = 0, instanceDeleted = 0, propertyUpdated = 0, noops = 0 }
	end
	extract("local function applyInstanceUpserts(", "local function applyInstanceDeletes(", with({
		liveHydrateEnabled = function() return true end,
		syncDesiredEntry = record("upsert"),
		resolveEntryParent = function() return renamed end,
	}), "applyInstanceUpserts")(change, {}, stats(), {})
	extract("local function applyInstanceDeletes(", "local function applyInstanceChange(", with({
		isEngineManagedContainerEntry = function() return false end,
	}), "applyInstanceDeletes")(change, {}, stats(), {})
	local reconcile = table.clone(change)
	reconcile.allowDeletes = true
	extract("local function applyInstanceReconcile(", "local function reconcileSessionKey(", with({
		clearMatchedSettingsInstances = function(serviceName) seen.clear = serviceName end,
		appendPreserveKeys = function(_, _, serviceName) seen.preserve = serviceName end,
		syncDesiredEntry = function(_, serviceName) seen.reconcile = serviceName return renamed end,
		recordDesiredStableEntry = record("stable"),
		keepUnknownsEnabled = function() return false end,
		removeUnknownInstances = function(serviceName, service) seen.prune, seen.pruned = serviceName, service end,
	}), "applyInstanceReconcile")(reconcile, {}, stats(), {})
	for _, name in { "upsert", "resolve", "clear", "preserve", "reconcile", "stable", "prune" } do
		assert(seen[name] == "ServerStorage", `A renamed service must keep its class name as the {name} path root`)
	end
	assert(seen.pruned == renamed)
end
print("Renamed service: upsert, delete and reconcile address the service by class name")

local applyProperty = extract("local function applyChangedProperty(", "local function resetProperty(",
	{ Workspace = {}, error = error }, "applyChangedProperty")
local moved = { GetFullName = function() return "StarterGui.Overlay" end }
local reparented, reparentError = pcall(applyProperty, moved, "Parent", { _type = "Ref", pathSegments = { "CoreGui" } }, {}, {}, { noops = 0 })
assert(not reparented and tostring(reparentError):find("Parent is not a property a push sets", 1, true),
	"a pushed Parent value must not move an instance")
local resetProperty = extract("local function resetProperty(", "local function deleteAttribute(",
	{ error = error }, "resetProperty")
local detached, detachError = pcall(resetProperty, moved, "Parent", {}, {}, { noops = 0 })
assert(not detached and tostring(detachError):find("Parent is not a property a push resets", 1, true),
	"a pushed Parent reset must not detach an instance")
