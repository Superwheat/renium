local fs = require("@lune/fs")
local luau = require("@lune/luau")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.lua")
local function extract(first, after, environment, returned)
	local start = assert(source:find(first, 1, true))
	local finish = assert(source:find(after, start, true))
	return luau.load(source:sub(start, finish - 1) .. "\nreturn " .. returned, { environment = environment })()
end
do
	local generationApi = {}
	local lookups = 0
	local service = {
		FindFirstChildWhichIsA = function() return nil end,
		FindFirstChild = function(_, name)
			lookups += 1
			return if name == "LuauLSP_Settings" then {} else nil
		end,
	}
	local generations = extract("\tfunction api.getServiceChangeGenerations(", "\tlocal function hasDirectPackageLink(", {
		api = generationApi,
		ctx = { allowedServices = { TestService = true }, studioChangeGeneration = function() return 7 end },
		game = { GetService = function(_, name) assert(name == "TestService"); return service end },
		denseArrayLength = extract("local function denseArrayLength(", "local PATH_SEPARATOR", {}, "denseArrayLength"),
	}, "api.getServiceChangeGenerations")
	local ordinary = generations({ services = { "TestService" } })
	assert(ordinary.generations.TestService == 7 and lookups == 0)
	local result = generations({ services = { "TestService" }, rootNames = { TestService = { "LuauLSP_Settings", "NewModel" } } })
	assert(result.hasPackageLinks.TestService == false and lookups == 2)
	assert(#result.absentRoots.TestService == 1 and result.absentRoots.TestService[1] == "NewModel")
	for _, roots in { false, { Workspace = { "x" } }, { TestService = { false } }, { TestService = { named = "x" } } } do
		assert(not pcall(generations, { services = { "TestService" }, rootNames = roots }))
	end
end
local lighting = { ClassName = "Lighting" }
local chat = { ClassName = "TextChatService" }
local material = { ClassName = "MaterialService" }
local starterPlayer = { ClassName = "StarterPlayer" }
local terrain = { ClassName = "Terrain" }
local workspace = { Terrain = terrain }
terrain.Parent = workspace
local predicate = extract("local function isNativeRootProperty(", "local function queueNativeRootWrite(", {
	Workspace = workspace,
	game = { GetService = function(_, name) return assert(({Lighting = lighting, TextChatService = chat, MaterialService = material, StarterPlayer = starterPlayer})[name]) end },
}, "isNativeRootProperty")
assert(predicate(lighting, "LightingStyle"))
assert(predicate(lighting, "PrioritizeLightingQuality"))
assert(predicate(terrain, "Decoration"))
assert(predicate(chat, "ChatVersion"))
assert(predicate(material, "Use2022Materials"))
for _, name in { "ModelStreamingBehavior", "StreamOutBehavior", "StreamingIntegrityMode", "StreamingTargetRadius", "UseNewLuauTypeSolver" } do
	assert(predicate(workspace, name))
	assert(not predicate({ ClassName = "Workspace" }, name))
end
assert(predicate(starterPlayer, "GameSettingsAvatar"))
assert(not predicate({ ClassName = "StarterPlayer" }, "GameSettingsAvatar"))
assert(not predicate(workspace, "SignalBehavior"), "Undo does not restore this engine field")
assert(not predicate(lighting, "Outlines"), "Undo does not restore this engine field")
assert(not predicate({ ClassName = "MaterialService" }, "Use2022Materials"))
assert(not predicate(material, "Use2022MaterialsXml"), "Serialized aliases must use the verified live setter")
assert(not predicate({ ClassName = "TextChatService" }, "ChatVersion"))
terrain.Name = "Renamed terrain"
workspace.Terrain = nil
assert(predicate(terrain, "Decoration"), "Terrain identity must not depend on its name")
assert(not predicate({ ClassName = "Lighting" }, "LightingStyle"), "Class names do not authorize foreign roots")
assert(not predicate(lighting, "Technology"), "Only verified setters are supported")
assert(not predicate(terrain, "Name"))
for _, name in { "MeshId", "MeshContent", "MeshSize", "CollisionFidelity", "RenderFidelity", "FluidFidelity" } do
	assert(predicate({ ClassName = "MeshPart" }, name))
	assert(not predicate({ ClassName = "Part" }, name))
end
assert(not predicate({ ClassName = "MeshPart" }, "TextureContent"))
assert(predicate({ ClassName = "SurfaceAppearance" }, "TexturePack"))
assert(not predicate({ ClassName = "MaterialVariant" }, "TexturePack"))
assert(not predicate({ ClassName = "SurfaceAppearance" }, "ColorMap"))

local queue = extract("local function queueNativeRootWrite(", "local function applyChangedProperty(", {
	decodePropertyValue = function(_, _, value) return true, value end,
	readProperty = function(instance, name) return instance[name] ~= nil, instance[name] end,
	valuesEqual = function(a, b) return a == b end,
	markLiveMutation = function(ctx) ctx.editorTransaction.mutated = true end,
}, "queueNativeRootWrite")
local ctx = { editorTransaction = { historyRecording = "owned-recording" } }
local stats = { noops = 0, propertyUpdated = 0 }
local change = { service = "Lighting", pathSegments = { "Lighting" }, pathOrdinals = { 1 } }
lighting.PrioritizeLightingQuality = false
queue(lighting, "PrioritizeLightingQuality", false, change, ctx, stats)
assert(stats.noops == 1 and ctx.editorTransaction.nativeRootWrites == nil)
queue(lighting, "PrioritizeLightingQuality", true, change, ctx, stats)
assert(stats.propertyUpdated == 1 and stats.nativeRootWrites[1].index == 1)
assert(ctx.editorTransaction.nativeRootWrites[1].instance == lighting)
assert(not pcall(queue, lighting, "PrioritizeLightingQuality", true, change, {}, stats))
assert(not pcall(queue, lighting, "PrioritizeLightingQuality", true, change, { editorTransaction = {} }, stats))

do
	local constructorDefault = { _type = "EnumItem", enumType = "CollisionFidelity", name = "Box" }
	local mesh = { ClassName = "MeshPart", CollisionFidelity = "Hull" }
	local destroyed, queued = 0, 0
	local reset = extract("local function resetProperty(", "local function deleteAttribute(", {
		Workspace = workspace,
		classHasProperty = function() return true end,
		Instance = { new = function(class)
			assert(class == "MeshPart")
			return { CollisionFidelity = constructorDefault, Destroy = function() destroyed += 1 end }
		end },
		readProperty = function(instance, name) return true, instance[name] end,
		isNativeRootProperty = predicate,
		queueNativeRootWrite = function(instance, name, value, target, context)
			assert(instance == mesh and name == "CollisionFidelity" and value == constructorDefault)
			assert(target == change and context == ctx)
			queued += 1
		end,
		writePropertyForSync = function() error("Protected resets must use the native setter") end,
	}, "resetProperty")
	ctx.serializeValue = function(value) return value end
	reset(mesh, "CollisionFidelity", change, ctx, stats, {})
	assert(queued == 1 and destroyed == 1 and mesh.CollisionFidelity == "Hull")
end

local sessions = { test = ctx.editorTransaction }
local timers = {}
local leaseActive = true
local operations = 0
local target = lighting
local function cancel(_, token) if token then token.active = false end end
local api = extract("\tfunction api.getTransactionState(", "\tfunction api.cancelRequestLease(", {
	api = {}, editorTransactions = sessions,
	transactionOutcomes = { get = function() return nil end },
	assertTransactionLease = function() assert(leaseActive, "cancelled lease") end,
	isNativeRootProperty = predicate,
	resolvePathSegments = function() return target end,
	cancelExpectedEvent = cancel,
	ctx = { samplePropertyChange = function(instance, name)
		assert(instance == lighting and name == "PrioritizeLightingQuality")
	end, expectPropertyEvent = function(instance, name, value)
		assert(instance == lighting and name == "PrioritizeLightingQuality" and value == true)
		return { active = true }
	end },
	beginSessionOperation = function() operations += 1 end,
	endSessionOperation = function(_, id, session)
		operations -= 1
		if session.expireRequested then sessions[id] = nil end
	end,
	task = { delay = function(seconds, callback) assert(seconds == 5) table.insert(timers, callback) end },
}, "api")
local function call(finish, changed)
	return api.getTransactionState({ transactionId = "test", nativeRootWrite = 1,
		finishNativeRootWrite = finish, nativeRootChanged = changed })
end
local session = sessions.test
assert(call(false).nativeRootWrite == 1 and operations == 1)
assert(not pcall(call, false), "Do not invoke a setter twice")
assert(call(true, true).state == "open" and operations == 0)
assert(session.nativeRootWrites[1].token.active, "Deferred own events must still match")
timers[1]()
assert(sessions.test == session, "An old timer must not roll back a completed write")
assert(not pcall(call, false), "Do not reuse completed native writes")

local function fresh()
	local write = session.nativeRootWrites[1]
	write.finished = nil
	write.token = nil
	session.expireRequested = nil
	sessions.test = session
end
fresh()
leaseActive = false
assert(not pcall(call, false) and operations == 0)
leaseActive = true
target = { ClassName = "Lighting" }
assert(not pcall(call, false) and operations == 0, "Refuse replaced targets")
target = lighting
call(false)
assert(operations == 1)
timers[#timers]()
assert(operations == 0 and sessions.test == nil, "Abandoned native writes must expire into rollback")
fresh()
call(false)
call(true, false)
assert(not session.nativeRootWrites[1].token.active, "No-op/failed writes must not mask later edits")
print("Native roots: exact targets, undo requirement, no-op routing, lease/identity checks, deferred events and abandoned-operation cleanup")

local attributes = { added = 29, changed = "after" }
local instance = { Quality = true, GetAttributes = function() return attributes end }
local tokens = {}
local undoContext = {
	expectAttributeEvent = function(_, name, value)
		local token = { active = true, name = name, value = value }
		table.insert(tokens, token)
		return token
	end,
	expectPropertyEvent = function(_, name, value)
		local token = { active = true, name = name, value = value }
		table.insert(tokens, token)
		return token
	end,
}
local snapshot = {
	groups = { {} },
	metadata = { { instance = instance, attributes = { changed = "before", removed = "original" } } },
	properties = { { instance = instance, name = "Quality", value = false } },
}
local transactionState = {}
transactionState.expectSnapshotUndo = extract("function TransactionState.expectSnapshotUndo(", "function TransactionState.rollbackSession(", {
	TransactionState = transactionState,
	valuesEqual = function(a, b) return a == b end,
	readProperty = function(object, name) return true, object[name] end,
}, "TransactionState.expectSnapshotUndo")
local drained = false
local frozen
local terrain = "outside"
transactionState.drainJournal = function()
	if drained then return {} end
	drained = true
	frozen = attributes.added
	return { { value = frozen } }
end
transactionState.prepareJournalRollback = function() end
transactionState.rollback = function(_, _, beforeReplace) beforeReplace() return {} end
transactionState.finishJournal = function() return { { value = frozen } } end
transactionState.replayJournal = function(records) attributes.added = records[1].value end
local rollback = extract("function TransactionState.rollbackSession(", "local NativeSerialization = {}", {
	TransactionState = transactionState,
	cancelExpectedEvent = cancel,
	Enum = { FinishRecordingOperation = { Cancel = "Cancel" } },
	finishHistoryRecording = function(recording, operation)
		if recording == nil then return end
		assert(drained, "Freeze outside edits before finishing engine history")
		if operation == "Cancel" then
			assert(#tokens == 4, "Expect added, removed and changed attributes, plus the property inverse")
			attributes.added = nil
			instance.Quality = false
			terrain = "before"
		else
			assert(#tokens == 0, "An untouched transaction must not expect any inverse writes")
		end
	end,
}, "TransactionState.rollbackSession")
for _, mutated in { true, false } do
	attributes.added = 29
	instance.Quality = true
	terrain = "outside"
	drained = false
	table.clear(tokens)
	rollback({ snapshot = snapshot, mutated = mutated, historyRecording = "recording" }, undoContext)
	assert(attributes.added == 29 and instance.Quality == not mutated)
	assert(terrain == if mutated then "before" else "outside", "Untouched rollback must preserve unobservable outside edits")
	for _, token in ipairs(tokens) do assert(not token.active) end
end
print("Rollback: preserve untouched Studio history and unobservable edits; freeze and replay outside edits around owned undo")

-- Explicit rollback owns its snapshots/retained roots, so cancellation must not
-- undo the unrelated Terrain history that property journals cannot observe.
for _, retained in { false, true } do
	local restored, replayed, committed = false, false, false
	local simpleState = {
		drainJournal = function() return {} end,
		prepareJournalRollback = function() end,
		expectSnapshotUndo = function() error("Explicit rollback must not invoke engine undo") end,
		rollback = function() restored = true return {} end,
		finishJournal = function() return {} end,
		replayJournal = function() assert(restored); replayed = true end,
	}
	local simpleRollback = extract("function TransactionState.rollbackSession(", "local NativeSerialization = {}", {
		TransactionState = simpleState,
		RunService = { Heartbeat = { Wait = function() end } },
		finishHistoryRecording = function(recording, operation)
			assert(recording == "recording" and operation == nil and restored and replayed,
				"Commit only after own values and outside edits are restored")
			committed = true
		end,
	}, "TransactionState.rollbackSession")
	local pending = { snapshot = {}, mutated = true, historyRecording = "recording" }
	if retained then pending.nativeUndo = { explicitRollback = true, prepared = {} } end
	simpleRollback(pending, {})
	assert(committed and pending.historyRecording == nil)
end

-- Exercise the production commit, not a second model of its phase ordering.
local pendingSessions = {}
local attached, documents, finished, generation = 0, 0, 0, 0
local commitContext = { studioChangeGeneration = function() return generation end }
local onFinish = nil
local commit = extract("local function commitTransactionUnsafe(", "function api.commitTransaction(", {
	editorTransactions = pendingSessions,
	pruneExpiredSessions = function() end,
	assertTransactionLease = function() end,
	captureOperationCancellation = function() return {} end,
	assertSessionOwnership = function() end,
	setTrackedOperationPhase = function() end,
	TransactionState = {
		drainJournal = function() return {} end,
		finishJournal = function() return {} end,
		replayJournal = function() end,
	},
	ReferenceOverlay = {
		chainReplacements = function() end,
		commitNative = function() attached += 1 end,
	},
	ScriptDocumentState = { apply = function() documents += 1 end },
	ctx = commitContext,
	cancelExpectedEvent = function() end,
	finishHistoryRecording = function() finished += 1;if onFinish then onFinish() end end,
	invalidateEditorService = function() end,
	recordTransactionOutcome = function(_, state, result) result.state = state return result end,
}, "commitTransactionUnsafe")
local function pending()
	local value = {
		historyRecording = "recording", snapshot = {}, instanceReplacements = {},
		nativeUndo = { replacements = {}, prepared = {} },
		postCommitPropertyChanges = {}, serviceNames = { "Workspace" },
		studioGenerations = { Workspace = 0 },
	}
	pendingSessions.snapshot = value
	return value
end
local prepared = pending()
for _ = 1, 2 do
	local response = commit({ transactionId = "snapshot", prepareOnly = true }, {})
	assert(response.state == "prepared" and response.committed == false)
	assert(prepared.nativeUndo ~= nil and prepared.historyRecording == "recording")
end
assert(attached == 1 and documents == 1 and finished == 0, "Snapshot sees staged contents without closing undo")
assert(commit({ transactionId = "snapshot" }, {}).state == "committed")
assert(attached == 1 and documents == 1 and finished == 1, "Finalization must not attach twice")
prepared = pending()
commit({ transactionId = "snapshot", prepareOnly = true }, {})
generation = 1
assert(not pcall(commit, { transactionId = "snapshot" }, {}), "Outside edits after snapshot preparation must reject publication")
assert(prepared.nativeUndo ~= nil and prepared.historyRecording == "recording")
assert(finished == 1, "Rejected snapshot must still be rollback-capable")
print("Protected snapshot: staged content, repeat preparation, single commit and intervening-edit rejection")

generation = 0
pending()
commitContext.capturePushProof = function() return generation end
onFinish = function() generation += 1 end
local committed = commit({ transactionId = "snapshot" }, {})
assert(committed.verifiedPushProof == 0 and generation == 1,
	"A post-commit callback must invalidate the cached proof, not become its baseline")
onFinish = nil
for _, interruption in ipairs({ "change", "cancel" }) do
	generation = 0
	local active = pending()
	local previouslyFinished = finished
	commitContext.capturePushProof = function()
		if interruption == "change" then generation += 1 else active.cancelRequested = true end
		return generation
	end
	assert(not pcall(commit, { transactionId = "snapshot" }, {}))
	assert(finished == previouslyFinished and active.historyRecording == "recording",
		"A change or cancellation during proof capture must leave rollback available")
end
print("Commit proof: post-commit edits invalidate equality; capture-time edits and cancellation retain rollback")

do
	local sessions = { inline = { transactionId = "transaction", nativeHeld = {}, leaseId = "lease" } }
	local completed = {}
	local receipts, finishes = 0, 0
	local inlineApi = {}
	inlineApi.appendBinaryImport = function(params)
		assert(params.importId == "inline" and params.nativeReceipt == true)
		assert(params.index == 1 and params.data == "receipt")
		receipts += 1
		return { ok = true }
	end
	local finish = extract("\tfunction api.finishBinaryImport(", "\tfunction api.cancelBinaryImport(", {
		api = inlineApi, binaryImports = sessions, completedBinaryImports = completed,
		editorTransactions = { transaction = {} }, COMPLETED_BINARY_IMPORT_TTL_SECONDS = 30,
		pruneExpiredSessions = function() end, pruneCompletedBinaryImports = function() end,
		assertTransactionLease = function() end,
		assertRequestLeaseActive = function(lease) assert(lease == "lease") end,
		currentRequestLeaseId = function() return "lease" end,
		captureOperationCancellation = function() return 1 end,
		assertSessionOwnership = function(generation) assert(generation == 1) end,
		armSessionExpiry = function() end, beginSessionOperation = function() end,
		endSessionOperation = function() end,
		finishNativeServiceImport = function()
			assert(receipts == 1, "Completion must validate the receipt first")
			finishes += 1
			return { ok = true, nativeInserted = true }
		end,
	}, "api.finishBinaryImport")
	assert(not pcall(finish, { importId = "inline", nativePhase = "prepare", nativeReceiptChunk = {} }))
	assert(not pcall(finish, { importId = "inline", nativePhase = "complete", nativeReceiptChunk = { index = 2, data = "receipt" } }))
	assert(receipts == 0 and finishes == 0 and sessions.inline ~= nil)
	local request = { importId = "inline", nativePhase = "complete", nativeReceiptChunk = { index = 1, data = "receipt" } }
	local response = finish(request)
	assert(response.nativeInserted and sessions.inline == nil)
	assert(finish(request) == response and receipts == 1 and finishes == 1,
		"A repeated completion must return its accepted result without applying the receipt again")
end
print("Inline native receipt: validation precedes completion; duplicate completion is idempotent")
