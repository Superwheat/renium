local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")
local identitySource = fs.readFile("tools/plugin_ws_bridge/BridgeIdentity.module.lua")
local Overlay = require("../BridgeReferenceOverlay.module")
local Retarget = require("../BridgeReferenceRetarget.module")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.lua")
local first = assert(source:find("local function cancelExpectedEvent(", 1, true))
local last = assert(source:find("local function setNameForSync(", first, true))

do
	local overlaySource = fs.readFile("tools/plugin_ws_bridge/BridgeReferenceOverlay.module.lua")
	local start = assert(overlaySource:find("local function indexPackagePresence(", 1, true))
	local finish = assert(overlaySource:find("local function initializePreparedGroup(", start, true))
	local index = luau.load(overlaySource:sub(start, finish - 1) .. "\nreturn indexPackagePresence")()
	local outside = {}
	local scans = 0
	local descendants = {}
	local function instance(className, parent)
		return {
			ClassName = className, Parent = parent,
			IsA = function(self, class) return self.ClassName == class end,
			GetChildren = function() error("Package indexing must not recursively enumerate every instance") end,
		}
	end
	local root = instance("Folder", outside)
	function root:GetDescendants() scans += 1; return table.clone(descendants) end
	local leaf = root
	for _ = 1, 5000 do
		leaf = instance("Folder", leaf)
		descendants[#descendants + 1] = leaf
	end
	local presence = {}
	index(root, presence)
	assert(scans == 1 and next(presence) == nil, "Package-free trees need no per-instance presence entries")
	local link = instance("PackageLink", leaf)
	local sibling = instance("Folder", root)
	descendants[#descendants + 1] = link
	descendants[#descendants + 1] = sibling
	descendants[#descendants + 1] = instance("PackageLink", leaf)
	descendants[#descendants + 1] = instance("PackageLink", root)
	index(root, presence)
	assert(scans == 2 and presence[root] and presence[link] and presence[leaf])
	assert(presence[outside] == nil and presence[sibling] == nil, "Only linked subtrees and their in-scope ancestors are marked")
	for index = 1, 5000 do assert(presence[descendants[index]], "Deep package ancestors must be indexed without recursion") end
	local direct = instance("PackageLink", outside)
	function direct:GetDescendants() return {} end
	index(direct, presence)
	assert(presence[direct] and presence[outside] == nil, "A PackageLink root must not mark its outside parent")
	local initEnd = assert(overlaySource:find("local function retainedRootChanged(", finish, true))
	local initialize = luau.load(overlaySource:sub(finish, initEnd - 1) .. "\nreturn initializePreparedGroup", {
		environment = {
			indexPackagePresence = function() error("Unretained roots need no package-presence scan") end,
		},
	})()
	local group = { outgoing = { root }, incoming = { sibling }, rootPaths = {}, incomingByPayloadIndex = {} }
	initialize(group)
	assert(group.outgoingRootSet[root] and group.packageScanRoots[1] == root)
	assert(group.packageScanRoots ~= group.outgoing, "Package safety checks retain their separate outgoing roots")
	local aliasStart = assert(overlaySource:find("local function retargetIncomingAliases(", 1, true))
	local aliasEnd = assert(overlaySource:find("local function captureOutgoingReferences(", aliasStart, true))
	local viewport = instance("Camera", nil)
	local duplicate = instance("Camera", nil)
	local retarget = luau.load(overlaySource:sub(aliasStart, aliasEnd - 1) .. "\nreturn retargetIncomingAliases", {
		environment = {
			isProtectedWorkspaceCameraInstance = function(value) return value == viewport end,
			BridgeReferenceRetarget = { apply = function() error("Explicit viewport repair must not walk reference properties") end },
		},
	})()
	assert(retarget({}, { [duplicate] = viewport }, {}, true) == 0)
	assert(not pcall(retarget, {}, { [root] = viewport }, {}, true), "Viewport policy cannot cover package aliases")
	assert(not pcall(retarget, {}, { [duplicate] = instance("Camera", nil) }, {}, true), "Only the actual viewport can use this policy")
end

-- Profiling scopes cannot leak after a failed setter or replace an outer scope.
do
	local setParent = luau.load(source:sub(first, last - 1) .. "\nreturn setParentForSync", {
		environment = { game = {}, Workspace = {} },
	})()
	local outer, profile, token = {}, {}, {}
	local scope = { attachment = outer }
	local cancelled = 0
	local completed = 0
	token.complete = function() completed += 1 end
	local ctx = {
		syncProfile = scope,
		expectParentChange = function() return token end,
		cancelExpectedEvent = function(value) assert(value == token); cancelled += 1 end,
	}
	for _, fail in { false, true } do
		local parent = nil
		local target = setmetatable({
			IsA = function() return false end,
			IsDescendantOf = function() return false end,
		}, {
			__index = function(_, key) return if key == "Parent" then parent else nil end,
			__newindex = function(_, key, value)
				assert(key == "Parent" and scope.attachment == profile)
				if fail then error("mock Parent setter failure") end
				parent = value
			end,
		})
		assert(pcall(setParent, target, {}, ctx, profile) == not fail)
		assert(scope.attachment == outer, "parenting restores profiling scope before propagating an error")
	end
	assert(cancelled == 1 and completed == 1, "observer preparation must complete only after successful parenting")
end

for _, postApplied in { false, true } do
for _, nested in { false, true } do
	local model = roblox.Instance.new("DataModel")
	local Identity = luau.load(identitySource, { environment = { game = model } })()
	local workspace = roblox.Instance.new("Folder")
	workspace.Name = "Workspace"
	workspace.Parent = model
	local instances = { model, workspace }
	local function canonical(value)
		-- Lune returns fresh userdata for Instance-valued reads; Studio returns
		-- the same table-key identity. Normalize only this mocked boundary.
		for _, known in instances do if known == value then return known end end
		return value
	end
	local function instance(class, name, parent)
		local value = roblox.Instance.new(class)
		value.Name = name
		value.Parent = parent
		instances[#instances + 1] = value
		return value
	end
	local holder = if nested then instance("Folder", "Holder", workspace) else workspace
	local camera = instance("Camera", "LiveViewport", holder)
	camera.FieldOfView = 70
	local oldChild = instance("StringValue", "Child", camera)
	oldChild.Value = "before"
	local extra = instance("Camera", "Camera", workspace)
	local outsidePointer = instance("ObjectValue", "OutsidePointer", model)
	outsidePointer.Value = camera
	local incomingHolder = if nested then instance("Folder", "Holder", nil) else nil
	local duplicate = instance("Camera", "SavedViewport", incomingHolder)
	duplicate.FieldOfView = 90
	local newChild = instance("StringValue", "Child", duplicate)
	newChild.Value = "after"
	local incomingExtra = instance("Camera", "Camera", nil)
	incomingExtra.FieldOfView = 85
	local pointer = instance("ObjectValue", "Pointer", nil)
	if not postApplied then pointer.Value = duplicate end
	local currentCamera = camera
	local workspaceProxy = setmetatable({ CurrentCamera = camera }, { __index = workspace })
	-- The production parenting function needs only CurrentCamera and workspace
	-- identity. Use the actual instance for parenting, with its camera read bound.
	local parentingSource = source:sub(first, last - 1):gsub("Workspace%.CurrentCamera", "currentCamera()")
	local setParent = luau.load(parentingSource .. "\nreturn setParentForSync", { environment = {
		game = model, Workspace = workspace, currentCamera = function() return currentCamera end,
	} })()
	local cameraAssignments = 0
	local referenceReads = 0
	local overlay = Overlay.create({
		isProtectedWorkspaceCameraInstance = function(value) return value == currentCamera end,
		BridgeIdentity = Identity,
		BridgeReferenceRetarget = Retarget,
		RbxDomModule = {
			getReferencePropertyNames = function(class) return if class == "ObjectValue" then { "Value" } else {} end,
			getObjectContentPropertyNames = function() return {} end,
		},
		captureExplorerSelection = function() return {} end,
		restoreExplorerSelection = function() end,
		containsPackageLink = function(root) return root:FindFirstChildWhichIsA("PackageLink", true) ~= nil end,
		pathCacheKey = Identity.pathCacheKey,
		resolveOrdinalChild = function(...) return canonical(Identity.resolveOrdinalChild(...)) end,
		readProperty = function(value, property) referenceReads += 1; return true, canonical(value[property]) end,
		writePropertyForSync = function(value, property, data) value[property] = data; return true end,
		setParentForSync = function(value, parent) setParent(value, parent, nil) end,
		removeInstanceForUndo = function(value) setParent(value, nil, nil) end,
		setCurrentCameraForSync = function(value)
			assert(value == camera, "Rollback must not activate a deserialized viewport")
			cameraAssignments += 1
		end,
	})
	local root = incomingHolder or duplicate
	local rootName = if nested then "Holder" else "SavedViewport"
	local viewportSegments = if nested then { "Workspace", "Holder", "SavedViewport" } else { "Workspace", "SavedViewport" }
	local viewportOrdinals = if nested then { 1, 1, 1 } else { 1, 1 }
	local group = {
		serviceName = "Workspace", service = workspaceProxy, target = workspace, targetPath = { "Workspace" },
		incoming = { root, incomingExtra, pointer }, incomingByPayloadIndex = { root, incomingExtra, pointer },
		outgoing = { if nested then holder else camera, extra },
		rootPaths = {
			{ pathSegments = { "Workspace", rootName }, pathOrdinals = { 1, 1 } },
			{ pathSegments = { "Workspace", "Camera" }, pathOrdinals = { 1, 1 } },
			{ pathSegments = { "Workspace", "Pointer" }, pathOrdinals = { 1, 1 } },
		},
		viewportCamera = { pathSegments = viewportSegments, pathOrdinals = viewportOrdinals },
		retainedRoots = {}, packageRoots = {},
	}
	local ctx = { studioChangeGeneration = function() return 0 end, invalidateService = function() end, allowedServices = {} }
	local undo = overlay.prepareRetained({ group }, ctx, true, postApplied)
	undo.prepared = { group }
	undo.currentCamera = camera
	undo.currentCameraParent = holder
	undo.generationsByService = {}
	undo.guard = { connections = {}, attributeCheckpoints = {} }
	assert(undo.replacements[camera] == camera, "Viewport replacement must be its existing identity")
	assert(undo.packageAliases[duplicate] == camera, "Viewport payload alias missing")
	if postApplied then
		assert(referenceReads == 0 and pointer.Value == nil, "Explicit viewport repairs must not scan all imported reference properties")
		pointer.Value = undo.resolveStagedPath(viewportSegments, viewportOrdinals)
	end
	assert(pointer.Value == camera, `Incoming viewport reference was not retargeted: {pointer.Value}`)
	assert(undo.resolveStagedPath(viewportSegments, viewportOrdinals) == camera)
	currentCamera = nil
	assert(not pcall(overlay.assertNativeImportState, undo, ctx), "Changed viewport must abort before commit")
	currentCamera = camera
	local generation = 0
	ctx.studioChangeGeneration = function() return generation end
	undo.generationsByService.Workspace = 0
	undo.guard.changedService = "Workspace"
	undo.guard.changedDetail = "outside structural edit"
	assert(not pcall(overlay.finishNativeStaging, undo, ctx), "Outside staging edits must abort before a mixed write")
	assert(undo.guard ~= nil, "Failed staging must retain its guard for rollback")
	undo.guard.changedService = nil
	overlay.finishNativeStaging(undo, ctx)
	assert(undo.guard == nil, "Expected retained-object edits must use the transaction journal")
	generation = 1
	assert(not pcall(overlay.assertNativeImportState, undo, ctx), "Outside edits after staging must still abort commit")
	generation = 0
	overlay.commitNative(undo, ctx)
	assert(camera.Name == "LiveViewport" and camera.FieldOfView == 70)
	assert(camera.Parent == (incomingHolder or workspace), `Retain viewport under replacement ancestor (nested={nested}, parent={camera.Parent}, mapped={undo.replacements[holder]})`)
	assert(newChild.Parent == camera and oldChild.Parent == nil and newChild.Value == "after")
	assert(incomingExtra.Parent == workspace and incomingExtra.FieldOfView == 85)
	assert(pointer.Value == camera and outsidePointer.Value == camera and duplicate.Parent == nil)
	overlay.rollbackNative(undo, ctx)
	assert(camera.Parent == holder and oldChild.Parent == camera and oldChild.Value == "before")
	assert(extra.Parent == workspace and incomingExtra.Parent == nil and newChild.Parent == nil)
	assert(outsidePointer.Value == camera and currentCamera == camera)
	-- The rollback helper is allowed to confirm the same camera, never a clone.
	assert(cameraAssignments <= 1)
	for _, value in { model, root, incomingExtra, pointer, duplicate, newChild } do value:Destroy() end
end
end
print("Native viewport retention: roots/nested ancestors, child sync, references and rollback passed")

do
	local service = roblox.Instance.new("Folder")
	service.Name = "TestService"
	local existing = roblox.Instance.new("ModuleScript")
	existing.Name, existing.Source, existing.Parent = "Existing", "return 1", service
	local incoming = roblox.Instance.new("Model")
	incoming.Name = "NewCar"
	local package = roblox.Instance.new("PackageLink")
	package.Parent = incoming
	local group = {
		serviceName = "TestService", service = service, target = service, targetPath = { "TestService" },
		additive = true, changeGeneration = 3, retainedRoots = {}, packageRoots = {},
		rootPaths = { { pathSegments = { "TestService", "NewCar" }, pathOrdinals = { 1, 1 } } },
		incoming = { incoming }, incomingByPayloadIndex = { incoming },
	}
	local finishStart = assert(source:find("\tfunction api.finishBinaryImport(", 1, true))
	local scanStart = assert(source:find("\t\t\tfor groupIndex, group in ipairs(session.groups) do", finishStart, true))
	local scanEnd = assert(source:find("\t\t\ttransaction.nativeGuard =", scanStart, true))
	local scan = luau.load("return function(session, ctx, ReferenceOverlay)\nlocal outgoingByGroup, generationsByService = {}, {}\n"
		.. source:sub(scanStart, scanEnd - 1) .. "\nreturn outgoingByGroup\nend", {
		environment = { ENGINE_MANAGED_CONTAINERS = {}, includeManagedInstance = function() return true end },
	})()
	local generation = 3
	local ctx = { studioChangeGeneration = function() return generation end, allowedServices = {}, invalidateService = function() end }
	local overlay = Overlay.create({
		RbxDomModule = { getReferencePropertyNames = function() return {} end, getObjectContentPropertyNames = function() return {} end },
		BridgeReferenceRetarget = Retarget,
		pathCacheKey = function(segments, ordinals) return table.concat(segments, "/") .. table.concat(ordinals, ",") end,
		captureExplorerSelection = function() return {} end, restoreExplorerSelection = function() end,
		setParentForSync = function(value, parent) value.Parent = parent end,
		removeInstanceForUndo = function() error("additive imports must not remove live roots") end,
	})
	local session = { groups = { group } }
	group.outgoing = scan(session, ctx, overlay)[1]
	assert(#group.outgoing == 0 and existing.Parent == service)
	generation = 4
	assert(not pcall(scan, session, ctx, overlay), "preflight generation must reject intervening edits")
	generation = 3
	incoming.Parent = service
	assert(not pcall(scan, session, ctx, overlay), "new-root collision must abort, not replace")
	incoming.Parent = nil
	group.rootPaths[1].pathOrdinals[2] = 2
	assert(not pcall(scan, session, ctx, overlay), "ambiguous new roots must abort")
	group.rootPaths[1].pathOrdinals[2] = 1
	for _ = 1, 100 do
		local undo = overlay.prepareRetained({ group }, ctx, true)
		undo.prepared, undo.generationsByService, undo.guard = { group }, { TestService = generation }, { connections = {}, attributeCheckpoints = {} }
		assert(not undo.needsReferenceRetarget and undo.removedRootCount == 0)
		assert(overlay.commitNative(undo, ctx) == 0)
		assert(incoming.Parent == service and package.Parent == incoming)
		assert(existing.Parent == service and existing.Source == "return 1")
		overlay.rollbackNative(undo, ctx)
		assert(incoming.Parent == nil and package.Parent == incoming and existing.Parent == service)
	end
	service:Destroy()
	incoming:Destroy()
end
print("Additive native import: collision/generation guards, PackageLinks and 100 identity-preserving commit/rollback cycles")
