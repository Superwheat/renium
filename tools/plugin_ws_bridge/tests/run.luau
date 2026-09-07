local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")

game = roblox.Instance.new("DataModel")
task = {
	delay = function() end,
}

local CandidateMatch = require("../BridgeCandidateMatch.module")
local Chunking = require("../BridgeChunking.module")
local Connection = require("../BridgeConnection.module")
local Content = require("../BridgeContent.module")
require("../BridgeCreatorApi.module")
local Identity = require("../BridgeIdentity.module")
local InstanceSwap = require("../BridgeInstanceSwap.module")
local ReferenceOverlay = require("../BridgeReferenceOverlay.module")
local ReferenceRetarget = require("../BridgeReferenceRetarget.module")
local Settings = require("../BridgeSettings.module")
local TransactionUpload = require("../BridgeTransactionUpload.module")
local Update = require("../BridgeUpdate.module")
local ValueEquality = require("../BridgeValueEquality.module")
require("./identity-workloads")
require("./connection-workloads")
require("./play-status-workloads")
require("./corner-change-workloads")
require("./session-lock-workloads")
require("./network-workloads")
require("./performance-workloads")
require("./microprofiler-workloads")
require("./microprofiler-analysis-workloads")

local function equal(actual, expected, label)
	if actual ~= expected then
		error(string.format("%s: expected %s, got %s", label, tostring(expected), tostring(actual)))
	end
end

local Runtime = require("../BridgePluginRuntime.module")
equal(type(Runtime.start), "function", "runtime module compilation")
do
	local deferred = {}
	task.defer = function(callback)
		deferred[#deferred + 1] = callback
	end
	local depth = 0
	local scope = {}
	local changes = {
		beginSuppress = function(_, receivedScope)
			equal(receivedScope, scope, "suppression scope forwarded")
			depth += 1
		end,
		endSuppress = function()
			depth -= 1
		end,
	}
	for _, shouldFail in { false, true } do
		local ok, result = pcall(Runtime.withSuppression, changes, function(params)
			equal(params, scope, "suppression params forwarded")
			if shouldFail then
				error("expected apply failure", 0)
			end
			return "applied"
		end, scope, scope)
		equal(ok, not shouldFail, "suppression preserves outcome")
		equal(result, if shouldFail then "expected apply failure" else "applied", "suppression result")
		equal(depth, 1, "suppression survives until deferred cleanup")
		table.remove(deferred, 1)()
		equal(depth, 0, "suppression cleans up after success and failure")
	end
	task.defer = nil
end
local editorSyncSource = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.lua")
do
	local first = assert(editorSyncSource:find("local function writePropertyForSync(", 1, true))
	local last = assert(editorSyncSource:find("local function setAttributeForSync(", first, true))
	local source = editorSyncSource:sub(first, last - 1) .. "\nreturn writePropertyForSync"
	for _, case in {
		{ property = "CollisionFidelity", class = "MeshPart", delay = 0, ok = true },
		{ property = "CollisionFidelity", class = "MeshPart", delay = 3, ok = true },
		{ property = "CollisionFidelity", class = "MeshPart", delay = 100, ok = false },
		{ property = "Transparency", class = "MeshPart", delay = 3, ok = false },
		{ property = "CollisionFidelity", class = "Part", delay = 3, ok = false },
	} do
		local waits, elapsed, cancelled = 0, 0, 0
		local current = "Box"
		local instance = { IsA = function(_, class) return class == case.class end }
		local write = luau.load(source, { environment = {
			writeProperty = function()
				if case.delay == 0 then current = "Hull" end
				return true, nil
			end,
			readProperty = function() return true, current end,
			valuesEqual = function(a, b) return a == b end,
			markLiveMutation = function() end,
			cancelExpectedEvent = function(_, token) equal(token, "expected", "write token"); cancelled += 1 end,
			os = { clock = function() return elapsed end },
			RunService = { Heartbeat = { Wait = function()
				waits += 1
				elapsed += 1
				if waits >= case.delay then current = "Hull" end
			end } },
		} })()
		local ok = write(instance, case.property, "Hull", {
			expectPropertyEvent = function() return "expected" end,
			assertSessionOwnership = function() end,
		})
		equal(ok, case.ok, "asynchronous collision write outcome")
		equal(cancelled, if case.ok then 0 else 1, "expected event retained until confirmed")
		equal(waits, if case.property == "CollisionFidelity" and case.class == "MeshPart" then math.min(case.delay, 3) else 0, "only asynchronous geometry waits, bounded")
	end
end
do
	local first = assert(editorSyncSource:find("function TransactionState.captureMeshGeometry(", 1, true))
	local last = assert(editorSyncSource:find("function TransactionState.captureProperties(", first, true))
	local captures, destroyed = 0, 0
	local mesh = { IsA = function(_, class) return class == "MeshPart" end }
	local state = {}
	luau.load(editorSyncSource:sub(first, last - 1), { environment = {
		TransactionState = state,
		isMeshGeometryProperty = function(name) return name == "UnscaledCofm" or name == "UnscaledVolume" end,
		readProperty = function() return false, nil end,
		Instance = { new = function()
			return {
				ApplyMesh = function(_, original) equal(original, mesh, "snapshot original geometry") end,
				Destroy = function() destroyed += 1 end,
			}
		end },
		SerializationService = { SerializeInstancesAsync = function(_, roots)
			equal(#roots, 1, "snapshot geometry only")
			captures += 1
			return "serialized geometry"
		end },
	} })()
	local seen, properties, unreadable = {}, {}, {}
	state.captureProperty(mesh, "Transparency", seen, properties, unreadable)
	equal(captures, 0, "ordinary mesh property needs no geometry snapshot")
	state.captureProperty(mesh, "UnscaledCofm", seen, properties, unreadable)
	state.captureProperty(mesh, "UnscaledVolume", seen, properties, unreadable)
	state.captureProperty(mesh, "CollisionFidelity", seen, properties, unreadable)
	equal(captures, 1, "one geometry snapshot for related writes")
	equal(destroyed, 1, "temporary mesh released")
	equal(#properties, 1, "unreadable fields retain geometry rollback")
	equal(properties[1].value, "serialized geometry", "rollback retains serialized values")
end
local editorSyncCompiled, editorSyncCompileError = pcall(luau.compile, editorSyncSource)
if not editorSyncCompiled then
	error("editor sync module compilation failed: " .. tostring(editorSyncCompileError))
end

do
	-- Exercise the production cache proof functions without a Studio serializer.
	local first = assert(editorSyncSource:find("local NativeSerialization = {}", 1, true))
	local last = assert(editorSyncSource:find("function NativeSerialization.appendIdentityCarriers", first, true))
	local proofs = {}
	local native = luau.load(editorSyncSource:sub(first, last - 1) .. "\nreturn NativeSerialization", {
		environment = { nativePayloadProofs = proofs },
	})()
	local revision = 10
	local tracking = true
	local session = {
		ctx = {
			isStudioChangeTracking = function() return tracking end,
			studioChangeGeneration = function() return 1 end,
			nativeExportGeneration = function() return revision end,
		},
		originalNonArchivableInstances = {}, cachedPayloadProofs = {},
		payloadHashes = {}, payloadSizes = {}, serializationJobs = {}, pendingPayloads = 0,
		payloadReadyEvent = { Fire = function() end },
	}
	for _ = 1, 100 do
		local proof = { generations = native.captureGenerations(session, { "Workspace" }), payloadHash = "hash", totalBytes = 42 }
		proofs.Workspace = proof
		native.appendJob(session, "Workspace", { "Workspace" }, {}, {})
		equal(session.cachedPayloadProofs.Workspace.proof, proof, "unchanged export reuses bytes")
		session.cachedPayloadProofs.Workspace = nil
		revision += 1 -- Camera structure/content changes, without a Live Sync edit.
		equal(native.proofMatches(session, proof), false, "ignored live edit still invalidates export")
		local jobs = #session.serializationJobs
		native.appendJob(session, "Workspace", { "Workspace" }, {}, {})
		equal(#session.serializationJobs, jobs + 1, "changed export serializes fresh bytes")
		equal(session.cachedPayloadProofs.Workspace, nil, "stale bytes are not offered")
	end
	tracking = false
	assert(native.captureGenerations(session, { "Workspace" }) == nil)
	assert(not native.proofMatches(session, proofs.Workspace))
end

do
	local Status = require("../BridgeStatus.module")
	local editor = { lastAtUnix = 0, requests = 0, lastMs = 0, sourceCreated = 0, sourceUpdated = 0,
		sourceDeleted = 0, instanceCreated = 0, instanceReplaced = 0, instanceDeleted = 0,
		propertyUpdated = 0, attributeUpdated = 0, noops = 0, errors = 0 }
	local state = { channels = { { open = true, ready = true } }, editorSyncStats = editor,
		connectionStatus = "Connected", connectRequested = true, pendingEditCount = 8, host = "localhost",
		ports = { "34872" }, bridgeVersion = "test", bridgeBuildUnix = 0, target = "test", runtimeId = "edit",
		codecVersion = 1, pendingReviewCount = 0 }
	equal(Status.view(state).title, "Connected", "older daemon status still works")
	editor.liveSync = { running = true, error = "native snapshot count mismatch" }
	local failed = Status.view(state)
	equal(failed.title, "Sync failed", "transport connection does not hide sync failure")
	equal(failed.syncFailed, true, "sync failure uses error indicator")
	assert(not failed.subtitle:find("native snapshot", 1, true), "raw diagnostic stays out of routine status")
	editor.liveSync = { running = true }
	equal(Status.view(state).title, "Connected", "successful retry clears failure")
	equal(Status.view(state).syncFailed, false, "successful retry clears error indicator")
	editor.liveSync = { running = false }
	equal(Status.view(state).subtitle, "Live Sync is off.", "stopped watcher is explicit")
	editor.liveSync = { running = true, paused = true }
	equal(Status.view(state).subtitle, "Live Sync is paused. Changes will wait.", "paused watcher is explicit")
end

local uploadedTransactions = {}
local uploads = TransactionUpload.create(function(transaction)
	uploadedTransactions[#uploadedTransactions + 1] = transaction
	return { ok = true }
end, function() end, ValueEquality.exactValuesEqual)
uploads.begin({ transactionId = "lease-upload", totalChunks = 1, rowCount = 1 }, "lease-a")
local wrongLeaseOk = pcall(uploads.append, {
	transactionId = "lease-upload",
	index = 1,
	rows = { { kind = "source", change = {} } },
}, "lease-b")
equal(wrongLeaseOk, false, "transaction upload rejects another lease")
uploads.append({
	transactionId = "lease-upload",
	index = 1,
	rows = { { kind = "source", change = {} } },
}, "lease-a")
equal(uploads.cancelLease("lease-b"), 0, "unrelated lease leaves upload")
equal(uploads.cancelLease("lease-a"), 1, "owning lease cancels upload")
local cancelledFinishOk = pcall(uploads.finish, { transactionId = "lease-upload" }, "lease-a")
equal(cancelledFinishOk, false, "cancelled transaction upload cannot finish")

uploads.begin({ transactionId = "integrity-upload", totalChunks = 1, rowCount = 1 }, "lease-a")
uploads.append({
	transactionId = "integrity-upload",
	index = 1,
	rows = { { kind = "source", change = { source = "original" } } },
}, "lease-a")
uploads.append({
	transactionId = "integrity-upload",
	index = 1,
	rows = { { kind = "source", change = { source = "original" } } },
}, "lease-a")
local conflictingRetryOk = pcall(uploads.append, {
	transactionId = "integrity-upload",
	index = 1,
	rows = { { kind = "source", change = { source = "different" } } },
}, "lease-a")
equal(conflictingRetryOk, false, "transaction upload rejects conflicting chunk retries")
uploads.finish({ transactionId = "integrity-upload" }, "lease-a")
equal(#uploadedTransactions, 1, "matching transaction chunk retry stays idempotent")
equal(uploadedTransactions[1].sourceChanges[1].source, "original", "conflicting retry keeps original chunk")

local queuedRequests = {
	{ id = 1, leaseId = "lease-a" },
	{ id = 2, leaseId = "lease-b" },
	{ id = 3, leaseId = "lease-a" },
}
local removedRequests = Connection.removeQueuedRequestsByLease(queuedRequests, "lease-a")
equal(#removedRequests, 2, "lease queue cancellation count")
equal(removedRequests[1].id, 1, "lease queue first cancelled request")
equal(removedRequests[2].id, 3, "lease queue second cancelled request")
equal(#queuedRequests, 1, "lease queue retained count")
equal(queuedRequests[1].id, 2, "lease queue retained order")

local outcomeNow = 10
local outcomeStore = Connection.createTransactionOutcomeStore(5, 2, function()
	return outcomeNow
end)
local committedOutcome = outcomeStore.record("tx-1", "committed", { undoRecorded = true })
equal(committedOutcome.state, "committed", "committed transaction state")
equal(committedOutcome.committed, true, "committed transaction outcome")
equal(committedOutcome.rolledBack, false, "committed transaction rollback flag")
local repeatedCommit = outcomeStore.get("tx-1")
equal(repeatedCommit.state, "committed", "lost commit response recovery")
repeatedCommit.state = "changed"
equal(outcomeStore.get("tx-1").state, "committed", "transaction outcome response isolation")
outcomeNow = 11
local rolledBackOutcome = outcomeStore.record("tx-2", "rolledBack", { replacements = 4 })
equal(rolledBackOutcome.rolledBack, true, "rolled back transaction outcome")
outcomeNow = 12
outcomeStore.record("tx-3", "committed", {})
equal(outcomeStore.get("tx-1"), nil, "transaction outcome capacity pruning")
outcomeNow = 18
equal(outcomeStore.get("tx-2"), nil, "transaction outcome expiry")
equal(outcomeStore.get("tx-3"), nil, "latest transaction outcome expiry")

local chunk = Chunking.chunkEncodedString("abcdefgh", 3, 3, -2)
equal(chunk.start, 3, "chunk start")
equal(chunk.nextStart, 6, "chunk next start")
equal(chunk.total, 8, "chunk total")
equal(chunk.chunk, "cde", "chunk body")
equal(chunk.pluginEncodeMs, 0, "chunk timing")

local exhausted = Chunking.chunkEncodedString("abc", 9, 4, 1)
equal(exhausted.chunk, "", "exhausted body")
equal(exhausted.nextStart, 9, "exhausted next start")

equal(Settings.normalizeLoopbackHost(" localhost "), "127.0.0.1", "localhost normalization")
equal(Settings.normalizeLoopbackHost("[::1]"), "::1", "IPv6 normalization")
equal(Settings.normalizeLoopbackHost("0.0.0.0"), nil, "remote host rejection")
equal(Settings.formatWebSocketUrl("::1", 8781), "ws://[::1]:8781", "IPv6 URL")

local ports = Settings.parsePortsCsv("8781, 8782,8781")
equal(#ports, 2, "port deduplication")
equal(ports[1], 8781, "first port")
equal(ports[2], 8782, "second port")
equal(Settings.parsePortsCsv("0,8781"), nil, "invalid port rejection")
equal(Settings.normalizeRuntimeSetting("changesThreshold", -3), 0, "number minimum")
equal(Settings.normalizeRuntimeSetting("diffLinesLimit", 2000000), 1000000, "number maximum")
equal(Settings.normalizeRuntimeSetting("initialSyncPriority", " RECONCILE "), "reconcile", "enum normalization")
equal(Settings.normalizeRuntimeSetting("initialSyncPriority", "bad"), "reconcile", "enum fallback")
equal(Update.isNewer("v0.1.4", "0.1.3"), true, "newer release")
equal(Update.isNewer("0.1.3", "0.1.3"), false, "same release")
equal(Update.isNewer("invalid", "0.1.3"), false, "invalid release")

local workspace = game:GetService("Workspace")
local folder = roblox.Instance.new("Folder")
folder.Name = "Parent"
folder.Parent = workspace
local child = roblox.Instance.new("Folder")
child.Name = "Child"
child.Parent = folder

local segments = Identity.getRefPathParts(child)
equal(#segments, 3, "reference path length")
equal(segments[1], "Workspace", "reference service")
equal(segments[2], "Parent", "reference parent")
equal(segments[3], "Child", "reference child")

local state = {
	instanceIdByInstance = { [child] = 42 },
	pathSegmentsByInstance = {},
	debugIdByInstance = {},
}
local reference = Identity.serializeRefValue(state, child)
equal(reference._type, "Ref", "reference type")
equal(reference.instanceId, "2a", "reference id")

equal(ValueEquality.valuesEqual(1, 1.00005), true, "number epsilon")
equal(ValueEquality.valuesEqual(1, 1.001), false, "number difference")
equal(ValueEquality.valuesEqual(0 / 0, 0 / 0), true, "NaN equality")
equal(
	ValueEquality.valuesEqual(roblox.Vector3.new(1, 2, 3), roblox.Vector3.new(1.00005, 2, 3)),
	true,
	"Vector3 epsilon"
)
equal(ValueEquality.valuesEqual(roblox.CFrame.new(1, 2, 3), roblox.CFrame.new(1.00005, 2, 3)), true, "CFrame epsilon")
equal(
	ValueEquality.valuesEqual(roblox.Color3.new(0.5, 0.25, 0.75), roblox.Color3.new(0.50001, 0.25, 0.75)),
	true,
	"Color3 quantization"
)
equal(
	ValueEquality.valuesEqual(roblox.UDim2.new(0.5, 2, 0.25, 4), roblox.UDim2.new(0.50005, 2, 0.25, 4)),
	true,
	"UDim2 epsilon"
)
equal(
	ValueEquality.valuesEqual(roblox.NumberRange.new(1, 2), roblox.NumberRange.new(1.00005, 2)),
	true,
	"NumberRange epsilon"
)
equal(
	ValueEquality.valuesEqual(
		roblox.PhysicalProperties.new(0.7, 0.3, 0.5, 1, 1),
		roblox.PhysicalProperties.new(0.70005, 0.3, 0.5, 1, 1)
	),
	true,
	"PhysicalProperties epsilon"
)
equal(
	ValueEquality.valuesEqual(
		roblox.NumberSequence.new({
			roblox.NumberSequenceKeypoint.new(0, 0, 0),
			roblox.NumberSequenceKeypoint.new(1, 1, 0),
		}),
		roblox.NumberSequence.new({
			roblox.NumberSequenceKeypoint.new(0, 0.00005, 0),
			roblox.NumberSequenceKeypoint.new(1, 1, 0),
		})
	),
	true,
	"NumberSequence epsilon"
)
equal(ValueEquality.valuesEqual({ value = 1 }, { value = 1.00005 }), true, "table epsilon")

local matchCandidates = {
	{ Value = "first", Marker = "one" },
	{ Value = "second", Marker = "two" },
}
local matchedCandidate = CandidateMatch.choose(
	matchCandidates,
	{ Value = "second" },
	{ Marker = "two" },
	function(candidate, name, value)
		return candidate[name] == value
	end,
	function(candidate, name, value)
		return candidate[name] == value
	end
)
equal(matchedCandidate, matchCandidates[2], "candidate property scoring")

local tiedCandidate = CandidateMatch.choose(
	matchCandidates,
	{ Target = { _type = "Ref", settingsId = "second" } },
	{},
	function(candidate, name, value)
		return candidate[name] == value
	end,
	function(candidate, name, value)
		return candidate[name] == value
	end
)
equal(tiedCandidate, nil, "candidate reference tie")

local oversizedCandidates = {}
for index = 1, 33 do
	oversizedCandidates[index] = { Value = index }
end
local cappedCandidate = CandidateMatch.choose(oversizedCandidates, { Value = 33 }, {}, function(candidate, name, value)
	return candidate[name] == value
end, function(candidate, name, value)
	return candidate[name] == value
end)
equal(cappedCandidate, nil, "candidate scoring cap")

local FakeInstance = {}
local FakeInstanceMetatable = {}
function FakeInstanceMetatable.__index(instance, key)
	if key == "Parent" then
		return rawget(instance, "_parent")
	end
	return FakeInstance[key] or rawget(instance, key)
end
function FakeInstanceMetatable.__newindex(instance, key, value)
	if key ~= "Parent" then
		rawset(instance, key, value)
		return
	end
	local parent = rawget(instance, "_parent")
	rawset(instance, "_parentWrites", rawget(instance, "_parentWrites") + 1)
	if parent ~= nil then
		table.remove(parent._children, table.find(parent._children, instance))
	end
	rawset(instance, "_parent", value)
	if value ~= nil then
		value._children[#value._children + 1] = instance
	end
end
function FakeInstance.new(className)
	return setmetatable({
		ClassName = className,
		Name = className,
		_attributes = {},
		_children = {},
		_parentWrites = 0,
	}, FakeInstanceMetatable)
end
function FakeInstance:GetChildren()
	return table.clone(self._children)
end
function FakeInstance:IsA(className)
	return self.ClassName == className
end
function FakeInstance:FindFirstChildWhichIsA(className, recursive)
	for _, childInstance in ipairs(self:GetChildren()) do
		if childInstance:IsA(className) then
			return childInstance
		end
		if recursive then
			local found = childInstance:FindFirstChildWhichIsA(className, true)
			if found ~= nil then
				return found
			end
		end
	end
	return nil
end
function FakeInstance:GetDescendants()
	local descendants = {}
	local function visit(instance)
		for _, childInstance in ipairs(instance:GetChildren()) do
			descendants[#descendants + 1] = childInstance
			visit(childInstance)
		end
	end
	visit(self)
	return descendants
end
function FakeInstance:GetAttributes()
	return table.clone(self._attributes)
end
function FakeInstance:SetAttribute(name, value)
	self._attributes[name] = value
end
function FakeInstance:GetAttribute(name)
	return self._attributes[name]
end
function FakeInstance:GetFullName()
	return self.Name
end
function FakeInstance:Destroy()
	self.Parent = nil
end

local tagsByInstance = {}
local tagService = {}
function tagService:GetTags(instance)
	return table.clone(tagsByInstance[instance] or {})
end
function tagService:AddTag(instance, tag)
	local tags = tagsByInstance[instance] or {}
	tagsByInstance[instance] = tags
	if not table.find(tags, tag) then
		tags[#tags + 1] = tag
	end
end
function tagService:RemoveTag(instance, tag)
	local tags = tagsByInstance[instance] or {}
	local index = table.find(tags, tag)
	if index then
		table.remove(tags, index)
	end
end
function tagService:HasTag(instance, tag)
	return table.find(tagsByInstance[instance] or {}, tag) ~= nil
end

local swapParent = FakeInstance.new("Folder")
local before = FakeInstance.new("Folder")
before.Name = "Value"
before.Parent = swapParent
local replaced = FakeInstance.new("Folder")
replaced.Name = "Value"
replaced:SetAttribute("Marker", "middle")
replaced.Parent = swapParent
tagsByInstance[replaced] = { "Synced" }
local nested = FakeInstance.new("Folder")
nested.Name = "Nested"
nested.Parent = replaced
local nestedTail = FakeInstance.new("Folder")
nestedTail.Name = "NestedTail"
nestedTail.Parent = replaced
local after = FakeInstance.new("Folder")
after.Name = "Value"
after.Parent = swapParent
local beforeParentWrites = before._parentWrites
local afterParentWrites = after._parentWrites

local replacement = InstanceSwap.replace(replaced, "StringValue", tagService, function(instance)
	instance.Parent = nil
end, FakeInstance.new)
equal(before.Parent, swapParent, "replacement first sibling parent")
equal(after.Parent, swapParent, "replacement trailing sibling parent")
equal(before._parentWrites, beforeParentWrites, "replacement first sibling untouched")
equal(after._parentWrites, afterParentWrites, "replacement trailing sibling untouched")
equal(replacement.Parent, swapParent, "replacement parent")
equal(nested.Parent, replacement, "replacement child parent")
equal(nestedTail.Parent, replacement, "replacement trailing child parent")
equal(replacement:GetAttribute("Marker"), "middle", "replacement attribute")
equal(tagsByInstance[replacement][1], "Synced", "replacement tag")

local nestedReplacement = InstanceSwap.replace(nested, "StringValue", tagService, function(instance)
	instance.Parent = nil
end, FakeInstance.new)
equal(nestedReplacement.Parent, replacement, "nested replacement parent")
equal(nestedTail.Parent, replacement, "nested replacement trailing sibling parent")

local referenceHolder = FakeInstance.new("ObjectValue")
referenceHolder.Reference = replaced
referenceHolder.Parent = swapParent
local referenceUpdated, referenceFailed = ReferenceRetarget.apply(
	{ swapParent },
	{ [replaced] = replacement },
	function(className)
		return if className == "ObjectValue" then { "Reference" } else {}
	end,
	function(instance, propertyName)
		return true, instance[propertyName]
	end,
	function(instance, propertyName, value)
		instance[propertyName] = value
		return true
	end
)
equal(referenceUpdated, 1, "replacement reference update")
equal(referenceFailed, 0, "replacement reference failure count")
equal(referenceHolder.Reference, replacement, "replacement reference target")

local lockedParent = FakeInstance.new("Folder")
local lockedTarget = FakeInstance.new("Folder")
lockedTarget.Parent = lockedParent
local lockedSibling = FakeInstance.new("Terrain")
lockedSibling.RobloxLocked = true
lockedSibling.Parent = lockedParent
local lockedSiblingParentWrites = lockedSibling._parentWrites
local lockedReplacementOk, lockedReplacement = pcall(InstanceSwap.replace, lockedTarget, "StringValue", tagService, function(instance)
	instance.Parent = nil
end, FakeInstance.new)
equal(lockedReplacementOk, true, "locked sibling replacement allowed")
equal(lockedTarget.Parent, nil, "locked sibling target removed")
equal(lockedReplacement.Parent, lockedParent, "locked sibling replacement parent")
equal(lockedSibling.Parent, lockedParent, "locked sibling retained")
equal(lockedSibling._parentWrites, lockedSiblingParentWrites, "locked sibling untouched")

local failedSwapParent = FakeInstance.new("Folder")
local failedSwapTarget = FakeInstance.new("Folder")
failedSwapTarget.Parent = failedSwapParent
local failedSwapChild = FakeInstance.new("Folder")
failedSwapChild.Parent = failedSwapTarget
local failedSwapOk, failedSwapError = pcall(
	InstanceSwap.replace,
	failedSwapTarget,
	"StringValue",
	tagService,
	function()
		error("remove failed")
	end,
	FakeInstance.new,
	function(instance, parent)
		if instance == failedSwapChild and parent == failedSwapTarget then
			error("restore failed")
		end
		instance.Parent = parent
	end
)
equal(failedSwapOk, false, "class swap rollback failure surfaced")
equal(string.find(failedSwapError, "rollback also failed", 1, true) ~= nil, true, "class swap rollback detail")

local rollbackParent = FakeInstance.new("Folder")
local rollbackOld = FakeInstance.new("Folder")
rollbackOld.Parent = rollbackParent
local rollbackIncoming = FakeInstance.new("Folder")
local incomingWasAttached = false
local removed = InstanceSwap.replaceChildren(
	{ { target = rollbackParent, preserved = {} } },
	{ { rollbackIncoming } },
	function(instance, parent)
		if instance == rollbackOld and parent == nil then
			incomingWasAttached = rollbackIncoming.Parent == rollbackParent
		end
		instance.Parent = parent
	end
)
equal(incomingWasAttached, true, "rollback replacement order")
equal(rollbackIncoming.Parent, rollbackParent, "rollback incoming parent")
equal(rollbackOld.Parent, nil, "rollback outgoing parent")
equal(removed[1].instance, rollbackOld, "rollback outgoing record")

local nativeGroups = {}
for _, serviceName in ipairs({ "Workspace", "ReplicatedStorage" }) do
	local target = roblox.Instance.new("Folder")
	local outgoing = roblox.Instance.new("Folder")
	outgoing.Parent = target
	nativeGroups[#nativeGroups + 1] = {
		incoming = { roblox.Instance.new("Folder") },
		outgoing = { outgoing },
		packageRoots = {},
		packageScanRoots = {},
		retainedLiveRoots = {},
		serviceName = serviceName,
		target = target,
	}
end
local allIncomingAttachedBeforeRemoval = true
local nativeOverlay = ReferenceOverlay.create({
	BridgeIdentity = {},
	BridgeReferenceRetarget = ReferenceRetarget,
	RbxDomModule = {},
	captureExplorerSelection = function()
		return {}
	end,
	containsPackageLink = function(instance)
		return instance:IsA("PackageLink") or instance:FindFirstChildWhichIsA("PackageLink", true) ~= nil
	end,
	pathCacheKey = Identity.pathCacheKey,
	removeInstanceForUndo = function(instance)
		for _, group in ipairs(nativeGroups) do
			allIncomingAttachedBeforeRemoval = allIncomingAttachedBeforeRemoval
				and group.incoming[1].Parent == group.target
		end
		instance.Parent = nil
	end,
	restoreExplorerSelection = function() end,
	setParentForSync = function(instance, parent)
		instance.Parent = parent
	end,
})
nativeOverlay.commitNative({
	generationsByService = {},
	guard = { connections = {} },
	prepared = nativeGroups,
	referenceUpdates = 0,
	replacements = {},
	retainedLiveRoots = {},
	outgoingScanRoots = {},
}, {
	allowedServices = {},
	invalidateService = function() end,
	studioChangeGeneration = function()
		return 0
	end,
})
equal(allIncomingAttachedBeforeRemoval, true, "all native replacements attached before removal")
for _, group in ipairs(nativeGroups) do
	equal(group.incoming[1].Parent, group.target, "native replacement retained")
	equal(group.outgoing[1].Parent, nil, "native original detached")
end

local packageRoot = roblox.Instance.new("Folder")
packageRoot.Name = "Package"
local packageTarget = roblox.Instance.new("Folder")
packageRoot.Parent = packageTarget
local packageLink = roblox.Instance.new("PackageLink")
packageLink.Parent = packageRoot
local duplicateRoot = roblox.Instance.new("Folder")
duplicateRoot.Name = "Package"
local packageMerges = {}
nativeOverlay.preparePackageMerge(packageRoot, duplicateRoot, { "Package" }, { 1 }, {}, packageMerges, {})
equal(#packageMerges, 1, "package merge prepared")
equal(#packageMerges[1].outgoing, 0, "package link excluded from outgoing content")
equal(packageLink.Parent, packageRoot, "missing package link preserved")

local packageStateOverlay = ReferenceOverlay.create({
	BridgeIdentity = {},
	BridgeReferenceRetarget = ReferenceRetarget,
	CollectionService = tagService,
	RbxDomModule = {
		getObjectContentPropertyNames = function()
			return {}
		end,
		getReferencePropertyNames = function()
			return {}
		end,
		getWritablePropertyNames = function()
			return { "Label" }
		end,
	},
	readProperty = function(instance, propertyName)
		return true, instance[propertyName]
	end,
	setAttributeForSync = function(instance, name, value)
		instance:SetAttribute(name, value)
		return true
	end,
	setTagForSync = function(instance, tag, added)
		if added then
			tagService:AddTag(instance, tag)
		else
			tagService:RemoveTag(instance, tag)
		end
	end,
	valuesEqual = function(left, right)
		return left == right
	end,
	writePropertyForSync = function(instance, propertyName, value)
		instance[propertyName] = value
		return true
	end,
})
local livePackageState = FakeInstance.new("Folder")
livePackageState.Label = "before"
livePackageState:SetAttribute("Changed", "before")
tagsByInstance[livePackageState] = { "Before" }
local desiredPackageState = FakeInstance.new("Folder")
desiredPackageState.Label = "after"
desiredPackageState:SetAttribute("Changed", "after")
desiredPackageState:SetAttribute("Added", true)
tagsByInstance[desiredPackageState] = { "After" }
local statePair = { live = livePackageState, duplicate = desiredPackageState }
packageStateOverlay.copyPackageRootState(statePair, {}, {}, false)
equal(livePackageState.Label, "after", "package state applied")
equal(livePackageState:GetAttribute("Added"), true, "package attribute applied")
equal(tagService:HasTag(livePackageState, "After"), true, "package tag applied")
packageStateOverlay.restorePackageRootState(statePair, {})
equal(livePackageState.Label, "before", "package state rolled back")
equal(livePackageState:GetAttribute("Changed"), "before", "package attribute rolled back")
equal(livePackageState:GetAttribute("Added"), nil, "package added attribute rolled back")
equal(tagService:HasTag(livePackageState, "Before"), true, "package original tag rolled back")
equal(tagService:HasTag(livePackageState, "After"), false, "package added tag rolled back")

equal(Content.serializeSource("None", nil), "", "empty content")
equal(Content.serializeSource("Uri", "rbxassetid://123"), "rbxassetid://123", "URI content")
local objectOk, objectError = pcall(Content.serializeSource, "Object", nil)
equal(objectOk, false, "object content rejection")
equal(string.find(objectError, "without losing data", 1, true) ~= nil, true, "object content error")
local opaqueOk = pcall(Content.serializeSource, "Opaque", nil)
equal(opaqueOk, false, "opaque content rejection")

print("Renium plugin behavior tests passed")
