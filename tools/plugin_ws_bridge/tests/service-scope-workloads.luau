local fs = require("@lune/fs")
local luau = require("@lune/luau")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.lua")
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
