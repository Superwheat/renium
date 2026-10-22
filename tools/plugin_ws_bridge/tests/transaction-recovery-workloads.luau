local base = require("./transaction-recovery-harness")
local harness, outsideNode = base.harness, base.outsideNode
local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")
-- Direct failures report failure immediately, but do not consume the two idle
-- attempts. Status reads do not renew a failed transaction in any starting path.
for _, mode in { "idle", "commit", "rollback", "lease", "cleanup" } do
	local h = harness()
	local s = h.begin("timing")
	local attempts = 0
	h.r.state.rollbackSession = function() attempts += 1; error("original parent missing", 0) end
	if mode == "commit" then
		s.cancelRequested = true -- real commit rejects before history; rollback still fails
		assert(not pcall(h.api.withRequestLease, "owner", false, h.api.commitTransaction, {transactionId = "timing"}))
	elseif mode == "rollback" then
		assert(not pcall(h.api.withRequestLease, "owner", false, h.api.rollbackTransaction, {transactionId = "timing"}))
	elseif mode == "lease" then
		h.api.cancelRequestLease("owner")
		h.api.finishRequestLeaseCancellation("owner")
	elseif mode == "cleanup" then
		h.api.cleanup()
	end
	assert(h.api.getTransactionState({transactionId = "timing"}).state == (if mode == "idle" then "open" else "rollbackFailed"), mode)
	if mode ~= "idle" then
		h.advance(60)
		assert(h.api.getTransactionState({transactionId = "timing"}).state == "rollbackFailed")
	end
	h.advance(120)
	assert(h.api.getTransactionState({transactionId = "timing"}).idleRollbackFailures == 1, mode)
	h.advance(180)
	assert(h.api.getTransactionState({transactionId = "timing"}).state == "rollbackFailed")
	h.advance(240)
	local result = h.api.getTransactionState({transactionId = "timing"})
	assert(result.state == "abandoned" and result.found and not result.committed and not result.rolledBack, mode)
	assert(attempts == (if mode == "idle" then 2 else 3), mode .. " attempts=" .. attempts)
	assert(h.r.recoveries.timing.session == s and h.r.sessions.timing == nil)
	assert(#h.expectations == 1)
	h.advance(600) -- recovery ownership is not the 300-second outcome cache
	assert(h.api.getTransactionState({transactionId = "timing"}).recovery ~= nil)
	h.begin("next")
end

-- Real native prepare, delayed receipt and completion, including failures of
-- receipt validation and observation teardown. None is an idle rollback failure.
for _, completionFailure in { "none", "receipt", "observations", "native" } do
	local h = harness()
	local s = h.begin("native")
	local native = { outgoing = {}, containers = {}, boundPaths = {}, containerSettings = {}, untaggedClasses = {}, referenceOverlay = {}, receivedCreated = 0, expectedCreated = 0 }
	local import = { transactionId = "native", leaseId = "owner", nativeHeld = native, groups = {}, updatedAt = 0 }
	h.r.imports.import = import
	local ready = h.api.withRequestLease("owner", false, h.api.finishBinaryImport, {importId = "import", nativePhase = "prepare", nativeHandoff = 1})
	assert(ready.nativeReaderReady and s.activeOperations == 1 and import.activeOperations == 1)
	h.api.withRequestLease("owner", false, h.api.finishBinaryImport, {importId = "import", nativePhase = "invoke"})
	h.advance(1000)
	assert(h.api.getTransactionState({transactionId = "native"}).state == "nativeOutcomeUnknown")
	assert(not s.rollbackFailed and not s.idleRollbackFailures)
	assert(not pcall(h.api.withRequestLease, "owner", false, h.api.rollbackTransaction, {transactionId = "native"}))
	assert(not s.rollbackFailed)
	local params = {importId = "import", nativePhase = "complete", nativeStatus = 4, nativeState = 14, nativeCreated = 0}
	if completionFailure == "receipt" then
		params.nativeState = 0
		assert(not pcall(h.api.withRequestLease, "owner", false, h.api.finishBinaryImport, params))
		params.nativeState = 14
	elseif completionFailure == "observations" then
		h.ctx.finishNativeImportObservations = function() error("observation still pending") end
		assert(not pcall(h.api.withRequestLease, "owner", false, h.api.finishBinaryImport, params))
		h.ctx.finishNativeImportObservations = function() return {} end
	elseif completionFailure == "native" then
		params.nativeStatus = 3
	end
	if completionFailure == "receipt" or completionFailure == "observations" then
		assert(s.activeOperations == 1 and import.activeOperations == 1 and not native.readerFinished)
	end
	local ok = pcall(h.api.withRequestLease, "owner", false, h.api.finishBinaryImport, params)
	assert(ok == (completionFailure ~= "native"))
	assert(native.readerFinished and s.activeOperations == 0 and import.activeOperations == 0)
end

-- Both pending markers protect expiry even without their ordinary counters.
for _, marker in { "pendingNativeRootWrite", "nativeServiceImport" } do
	local h = harness(); local s = h.begin(marker)
	s[marker] = { readerFinished = false }
	h.advance(600)
	assert(h.r.sessions[marker] == s and not s.rollbackFailed and not s.idleRollbackFailures)
	assert(#h.delayed == 0, "pending native work must not continually re-arm idle timers")
end

-- An expiry that yields owns the operation gate and its session. Prune and
-- same-session calls cannot abandon it or start a second rollback.
do
	local h = harness(); local s = h.begin("overlap")
	local attempts = 0
	h.r.state.rollbackSession = function()
		attempts += 1
		if attempts == 2 then coroutine.yield(); return {} end
		error("failed", 0)
	end
	h.advance(120)
	h.now = 241
	local timer = table.remove(h.delayed, 1)
	timer.run() -- queue exclusive work
	local running = coroutine.create(table.remove(h.delayed, 1).run)
	assert(coroutine.resume(running))
	assert(coroutine.status(running) == "suspended" and s.activeOperations == 1 and h.gate.busy())
	assert(not h.r.expire(h.r.sessions, "overlap", s))
	assert(attempts == 2 and h.r.sessions.overlap == s)
	h.gate.resume(h.gate.threads[#h.gate.threads])
	assert(h.api.getTransactionState({transactionId = "overlap"}).state == "rolledBack")
	h.begin("next")
end

-- Structural failure after the REAL prepareJournalRollback detaches an outside
-- root. A same-path impostor cannot become its parent; the original may return.
do
	local h = harness(); local s = h.begin("outside")
	local target = outsideNode(h, "OriginalParent", h.realGame:GetService("Workspace"))
	local root = outsideNode(h, "ManualAddition", target)
	target.children = { root }
	s.mutated = true
	s.snapshot.groups = { {target = target, preserved = {}} }
	local record = {instance = root, currentInstance = root, parent = target, service = "Workspace", properties = {}, attributes = {}, structural = true}
	s.changeJournal = { record }
	h.r.state.rollback = function()
		target.Parent = nil
		error("original parent is missing during rollback", 0)
	end
	h.advance(240)
	assert(h.api.getTransactionState({transactionId = "outside"}).state == "abandoned")
	assert(root.Parent == nil and s.preservedJournalRoots[root] and s.changeJournal[1] == record)
	assert(h.r.recoveries.outside.session.snapshot == s.snapshot)
	h.advance(360)
	assert(root.Parent == nil and #s.changeJournal == 1)
	target.Parent = h.realGame:GetService("Workspace")
	h.begin("newer")
	h.advance(480)
	assert(root.Parent == nil, "recovery must wait while a newer transaction owns the place")
	h.api.withRequestLease("owner", false, h.api.rollbackTransaction, {transactionId = "newer"})
	h.advance(600)
	assert(root.Parent == target and #s.changeJournal == 0 and not s.preservedJournalRoots[root])
end

-- Native originals remain owned in the holding folder. Only an empty folder
-- can be destroyed; failed destruction retains its handle and diagnostic.
do
	local h = harness(); local s = h.begin("holding")
	local core = h.realGame:GetService("CoreGui")
	local holding = outsideNode(h, "Renium Transaction", core)
	local target = outsideNode(h, "OriginalParent", nil)
	local root = outsideNode(h, "Original", holding)
	holding.children = { root }
	s.nativeUndo = {retainedParent = holding, replacements = {}, prepared = {{outgoing = {root}, target = target}}}
	h.r.state.rollbackSession = function() error("restoration rejected", 0) end
	h.advance(240)
	local result = h.api.getTransactionState({transactionId = "holding"})
	assert(result.recovery.retainedNativeRoots == 1 and root.Parent == holding and s.nativeUndo.retainedParent == holding)
	holding.failDestroy = true
	target.Parent = h.realGame:GetService("Workspace")
	h.advance(360)
	assert(root.Parent == target and s.nativeUndo.retainedParent == holding and not holding.destroyed)
	assert(s.cleanupErrors["recovery holding folder"] ~= nil)
	holding.failDestroy = false
	h.advance(480)
	assert(holding.destroyed and s.nativeUndo.retainedParent == nil)
end

for _, failure in { "failHistory", "failJournal", "failInvalidate", "failExpectation" } do
	local h = harness(); local s = h.begin(failure)
	h[failure] = true
	h.r.state.rollbackSession = function() error("rollback failed", 0) end
	h.advance(240)
	assert(h.api.getTransactionState({transactionId = failure}).state == "abandoned")
	assert(h.r.recoveries[failure].session == s and #h.r.failures >= 3)
	if failure == "failHistory" then assert(s.historyRecording ~= nil) end
	if failure == "failJournal" then assert(s.journalActive) end
	if failure == "failExpectation" then assert(s.expectationPending and #h.expectations == 0)
	else assert(#h.expectations == 1) end
	h[failure] = false
	h.advance(360)
	assert(s.historyRecording == nil and not s.journalActive and not s.expectationPending)
	assert(#h.expectations == 1)
end

-- Prune and timer share the same >= deadline and attempt budget.
do
	local h = harness(); h.begin("prune")
	h.r.state.rollbackSession = function() error("failed", 0) end
	h.now = 119; assert(not pcall(h.begin, "next") and not h.r.sessions.prune.rollbackFailed)
	h.now = 120; assert(not pcall(h.begin, "next") and h.r.sessions.prune.idleRollbackFailures == 1)
	h.now = 240; h.begin("next")
	assert(h.api.getTransactionState({transactionId = "prune"}).state == "abandoned")
end

-- Healthy refusal does not diagnose rollback failure. Post-fence failure stays
-- committed, while successful lease-free expiry publishes rolledBack.
do
	local h = harness(); h.begin("healthy")
	local ok, detail = pcall(h.begin, "next")
	assert(not ok and tostring(detail):find("current operation", 1, true))
	h.failInvalidate = true
	local committed = h.api.withRequestLease("owner", false, h.api.commitTransaction, {transactionId = "healthy"})
	assert(committed.state == "committed")
	h.advance(240)
	assert(h.api.getTransactionState({transactionId = "healthy"}).state == "committed")
	local h2 = harness(); h2.begin("lease-free")
	h2.advance(120)
	assert(h2.api.getTransactionState({transactionId = "lease-free"}).state == "rolledBack")
	h2.advance(420)
	assert(h2.api.getTransactionState({transactionId = "lease-free"}).state == "notFound")
end

-- Cancellation during an active operation attempts rollback at completion, but
-- that immediate attempt is not one of the two qualifying idle failures.
do
	local h = harness(); local s = h.begin("busy")
	h.r.state.rollbackSession = function() error("failed", 0) end
	h.r.beginOp(s); s.expireRequested = true
	h.advance(500)
	assert(not s.rollbackFailed)
	h.r.endOp(h.r.sessions, "busy", s)
	h.advance(500)
	assert(s.rollbackFailed and (s.idleRollbackFailures or 0) == 0)
	h.advance(620); assert(s.idleRollbackFailures == 1)
	h.advance(740); assert(h.api.getTransactionState({transactionId = "busy"}).state == "abandoned")
end

do
	local h = harness(); local s = h.begin("lease-owner")
	local attempts = 0
	h.r.state.rollbackSession = function() attempts += 1; return {} end
	assert(not pcall(h.api.withRequestLease, "other", false, h.api.commitTransaction, {transactionId = "lease-owner"}))
	assert(attempts == 0 and h.r.sessions["lease-owner"] == s and not s.rollbackFailed)
end

-- Scheduler lateness and rollback duration shift the next complete idle interval.
do
	local h = harness(); local s = h.begin("late")
	h.r.state.rollbackSession = function() h.now += 30; error("slow rollback failure", 0) end
	h.now = 200; table.remove(h.delayed, 1).run()
	table.remove(h.delayed, 1).run()
	assert(s.updatedAt == 230 and s.idleRollbackFailures == 1)
	h.advance(349); assert(h.r.sessions.late == s)
	h.advance(350); assert(h.api.getTransactionState({transactionId = "late"}).state == "abandoned")
end

-- Even an artificially revoked owner cannot overwrite a later terminal result
-- when its suspended rollback resumes.
do
	local h = harness(); h.begin("stale")
	h.r.state.rollbackSession = function() coroutine.yield(); return {} end
	h.now = 120; table.remove(h.delayed, 1).run()
	local suspended = coroutine.create(table.remove(h.delayed, 1).run)
	assert(coroutine.resume(suspended))
	h.r.sessions.stale = nil
	h.api.reviewRecordOutcome("stale", "abandoned", {rollbackError = "newer terminal owner"})
	h.gate.resume(h.gate.threads[#h.gate.threads])
	assert(h.api.getTransactionState({transactionId = "stale"}).rollbackError == "newer terminal owner")
end

-- Publication is visible even while expectation release yields.
do
	local h = harness(); h.begin("publication")
	h.ctx.finishEditorTransactionExpectation = function() coroutine.yield() end
	h.now = 120; table.remove(h.delayed, 1).run()
	local suspended = coroutine.create(table.remove(h.delayed, 1).run)
	assert(coroutine.resume(suspended))
	assert(h.gate.busy() and h.api.getTransactionState({transactionId = "publication"}).state == "rolledBack")
	h.gate.resume(h.gate.threads[#h.gate.threads])
end

-- A real rejected history Cancel remains owned; terminal Commit failure is
-- diagnosed independently and retried without claiming content restoration.
do
	local h = harness(); local s = h.begin("history")
	s.mutated = true
	s.snapshot.groups = {{target = outsideNode(h, "Target", nil), preserved = {}}}
	h.failHistory = true
	h.advance(240)
	assert(h.api.getTransactionState({transactionId = "history"}).state == "abandoned")
	assert(s.historyRecording ~= nil and s.cleanupErrors["recovery history finish"])
	assert(h.history[#h.history - 1].operation == roblox.Enum.FinishRecordingOperation.Cancel)
	assert(h.history[#h.history].operation == roblox.Enum.FinishRecordingOperation.Commit)
end

-- Successful commit retains failed disposal handles, publishes anyway, and
-- retries each resource until both the root and its empty folder are released.
do
	local h = harness(); local s = h.begin("commit-cleanup")
	local holding = outsideNode(h, "Holding", h.realGame:GetService("CoreGui"))
	local root = outsideNode(h, "Staging", holding)
	holding.children = {root}; root.failDestroy = true
	s.nativeUndo = {retainedParent = holding, prepared = {{outgoing = {root}}}, replacements = {}}
	s.commitPrepared = true
	local outcome = h.api.withRequestLease("owner", false, h.api.commitTransaction, {transactionId = "commit-cleanup"})
	assert(outcome.state == "committed" and outcome.cleanupErrors and #h.expectations == 1)
	assert(h.r.recoveries["commit-cleanup"].session.nativeUndo.retainedParent == holding)
	root.failDestroy = false
	h.advance(240)
	assert(root.destroyed and holding.destroyed and s.nativeUndo.retainedParent == nil)
	assert(h.r.recoveries["commit-cleanup"] == nil)
end

-- Runtime dispatch serializes mutating state queries using the same gate.
do
	local source = fs.readFile("tools/plugin_ws_bridge/BridgePluginRuntime.module.luau")
	luau.compile(source)
	local first = assert(source:find("isExclusiveMethod = function(method, params)", 1, true))
	local last = assert(source:find("isSessionOwnedMethod = function", first, true))
	local dispatch = luau.load("return {" .. source:sub(first, last - 1) .. "}", {
		environment = {Config = {bridgeExclusiveMethods = {beginEditorTransaction = true}}},
	})().isExclusiveMethod
	assert(not dispatch("getEditorTransactionState", {}))
	for _, parameter in {"nativeRootWrite", "nativeTerrainBaseline", "meshGeometry"} do
		assert(dispatch("getEditorTransactionState", {[parameter] = true}))
	end
end

print("Full-module transaction recovery workloads passed")
