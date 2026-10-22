local base = require("./transaction-recovery-harness")
local harness, outsideNode = base.harness, base.outsideNode

local function publicationAfterRevocation(mutate)
	local h = harness(mutate)
	local s = h.begin("publication-owner")
	local attempts = 0
	h.r.state.rollbackSession = function()
		attempts += 1
		h.owns = false
		return {}
	end
	h.api.cancelRequestLease("owner")
	h.api.finishRequestLeaseCancellation("owner")
	assert(s.activeOperations == 0 and not h.gate.busy() and h.getGeneration() == nil)
	assert(h.r.sessions[s.transactionId] == s and s.pendingTerminal.state == "rolledBack")
	assert(#h.expectations == 0 and s.rollbackFailed:find("ownership was lost", 1, true))
	h.owns = true
	h.advance(120)
	assert(attempts == 1, "retry publication without replaying a completed rollback")
	assert(h.api.getTransactionState({transactionId = s.transactionId}).state == "rolledBack")
	assert(s.activeOperations == 0 and #h.expectations == 1 and h.getGeneration() == nil)
end
publicationAfterRevocation()

-- Error rendering is fallible too; it cannot retain the operation fence.
do
	local h=harness(); local s=h.begin("cancellation-diagnostic")
	s.onExpire=function()
		error(setmetatable({}, {__tostring=function() error("diagnostic failed",0) end}),0)
	end
	h.api.cancelRequestLease("owner")
	assert(not pcall(h.api.finishRequestLeaseCancellation,"owner"))
	assert(s.activeOperations==0 and not h.gate.busy() and h.getGeneration()==nil)
end

do
	local h=harness()
	h.gate.call(function()
		assert(h.getGeneration()==7)
		assert(not pcall(h.ctx.withSessionOwnership,7,function() error("callback failed",0) end))
		assert(h.getGeneration()==7, "callback failure must restore the previous binding")
		assert(not pcall(h.ctx.withSessionOwnership,8,function() error("must not enter",0) end))
		assert(h.getGeneration()==7, "validation failure must restore the previous binding")
	end)
	assert(h.getGeneration()==nil)
end

-- Publication may yield through expectation release. Losing ownership there
-- stops replay but cannot prevent the already-armed retention deadline.
do
	local h=harness(); local s=h.begin("lost-during-transfer")
	h.r.state.rollbackSession=function() error("failed",0) end
	h.advance(120)
	h.ctx.finishEditorTransactionExpectation=function() h.owns=false end
	h.advance(240)
	assert(h.r.recoveries[s.transactionId] and s.activeOperations==0 and not h.gate.busy())
	h.advance(2040)
	assert(h.r.recoveries[s.transactionId]==nil and h.getGeneration()==nil)
	assert(h.api.getTransactionState({transactionId=s.transactionId}).recovery.state=="expired")
end

-- An idle healthy transaction publishes under the captured generation, even
-- though there is no request lease on the timer coroutine.
do
	local h = harness(); h.begin("idle-owner")
	h.advance(240)
	assert(h.api.getTransactionState({transactionId = "idle-owner"}).state == "rolledBack")
	assert(not h.gate.busy() and h.getGeneration() == nil and #h.expectations == 1)
end

-- Recovery cannot mutate under a revoked Studio lock. Prune binds and validates
-- the same generation; local exclusivity alone is insufficient.
for _, changedGeneration in {false,true} do
	local h = harness(); local s = h.begin("revoked")
	local parent = outsideNode(h, "Parent", nil)
	local root = outsideNode(h, "Manual", nil)
	s.changeJournal = {{instance=root,currentInstance=root,parent=parent,service="Workspace",structural=true,properties={},attributes={}}}
	s.preservedJournalRoots = {[root]=true}
	h.r.state.rollbackSession = function() error("parent missing", 0) end
	h.advance(240)
	parent.Parent = h.realGame:GetService("Workspace")
	if changedGeneration then h.changeOwnershipGeneration(8) else h.owns = false end
	h.advance(360)
	assert(root.Parent == nil and #s.changeJournal == 1 and h.getGeneration() == nil)
	assert(not h.gate.busy())
	local h2 = harness(); local s2 = h2.begin("prune-revoked")
	local writes = 0
	h2.r.state.rollbackSession = function() writes += 1; return {} end
	h2.now = 120; h2.owns = false
	h2.r.prune(h2.r.sessions)
	assert(writes == 0 and h2.r.sessions[s2.transactionId] == s2 and h2.getGeneration() == nil)
end

-- Real queue overlap: queued begin cannot enter while rollback is suspended.
do
	local h = harness(); h.begin("queue")
	local attempts = 0
	h.r.state.rollbackSession = function() attempts += 1; coroutine.yield(); return {} end
	h.now = 120; table.remove(h.delayed, 1).run()
	local waiter = h.gate.spawn(table.remove(h.delayed, 1).run)
	local worker = h.gate.threads[#h.gate.threads]
	local entered = false
	h.gate.request(function() entered = true end)
	h.r.prune(h.r.sessions)
	assert(not entered and attempts == 1 and h.gate.busy())
	h.gate.resume(worker)
	assert(entered and not h.gate.busy() and coroutine.status(waiter) == "dead")
	assert(h.getGeneration() == nil and #h.expectations == 1)
end

local function prepared(h, id)
	local s = h.begin(id)
	-- Empty fixture snapshot has no engine graph to serialize or restore.
	h.r.state.topologyMatchesSnapshot = function() return true end
	local native = {outgoing={},containers={},boundPaths={},containerSettings={},untaggedClasses={},referenceOverlay={},receivedCreated=0,expectedCreated=0}
	local import = {transaction=s,transactionId=id,leaseId="owner",nativeHeld=native,groups={},updatedAt=0,
		withOwnership=s.withOwnership,scheduleExpiry=s.scheduleExpiry}
	h.r.imports.import = import
	h.api.withRequestLease("owner",false,h.api.finishBinaryImport,{importId="import",nativePhase="prepare",nativeHandoff=1})
	return s, native, import
end

local function duplicateInvoke(mutate)
	local h = harness(mutate)
	prepared(h, "duplicate-invoke")
	local params = { importId = "import", nativePhase = "invoke" }
	assert(h.api.withRequestLease("owner", false, h.api.finishBinaryImport, params).nativeInvocationAuthorized)
	assert(not pcall(h.api.withRequestLease, "owner", false, h.api.finishBinaryImport, params), "duplicate invocation accepted")
end

local function invokeDeadline(mutate)
	for _, time in { 119.999, 120, 120.001 } do
		local h = harness(mutate)
		local s, native, import = prepared(h, "invoke-deadline")
		h.now = time -- Deliberately do not run the expiry callback.
		local ok = pcall(h.api.withRequestLease, "owner", false, h.api.finishBinaryImport, { importId = "import", nativePhase = "invoke" })
		assert(ok == (time < 120), "invoke deadline gate failed before timer cleanup")
		assert((native.invocationAuthorized == true) == (time < 120))
		assert(s.activeOperations == 1 and import.activeOperations == 1 and not native.readerFinished)
	end
end

local function completeWithoutInvoke(mutate)
	local h = harness(mutate)
	local s, native, import = prepared(h, "complete-without-invoke")
	local ok, detail = pcall(h.api.withRequestLease, "owner", false, h.api.finishBinaryImport,
		{ importId = "import", nativePhase = "complete", nativeStatus = 4, nativeState = 14, nativeCreated = 0 })
	assert(not ok and tostring(detail):find("never authorized", 1, true), "completion without invoke accepted")
	assert(s.activeOperations == 1 and import.activeOperations == 1 and not native.readerFinished)
end

for _, case in { duplicateInvoke, invokeDeadline, completeWithoutInvoke } do case() end
for _, mutation in {
	{
		before = '\t\t\tif not native.readerArmed or native.readerFinished or native.invocationAuthorized\n\t\t\t\tor os.clock() >= native.prepareDeadline then\n\t\t\t\terror("Native invocation permission expired or was already consumed")\n\t\t\tend\n',
		cases = { duplicateInvoke, invokeDeadline },
	},
	{
		before = '\t\tif not native.invocationAuthorized then\n\t\t\terror("Native invocation was never authorized")\n\t\tend\n',
		cases = { completeWithoutInvoke },
	},
} do
	for _, case in mutation.cases do
		local mutated = false
		local ok, detail = pcall(case, function(source)
			local first, last = source:find(mutation.before, 1, true)
			assert(first, "invoke mutation target missing")
			mutated = true
			return source:sub(1, first - 1) .. source:sub(last + 1)
		end)
		assert(mutated and not ok and not tostring(detail):find("mutation target missing", 1, true), "invoke gate mutation survived")
	end
end

do
	local h = harness()
	local s, native = prepared(h, "slow-daemon")
	h.advance(100)
	h.api.getTransactionState({ transactionId = s.transactionId })
	h.advance(120)
	assert(native.readerFinished and h.api.getTransactionState({ transactionId = s.transactionId }).rolledBack)
end

-- A killed daemon before invoke leaves proof nothing ran. At the inclusive
-- deadline prepare is disarmed and normal rollback can release the lock.
do
	local h = harness(); local s, native, import = prepared(h, "not-invoked")
	h.api.cancelRequestLease("owner"); h.api.finishRequestLeaseCancellation("owner"); h.api.cleanup()
	h.advance(119.999)
	assert(s.activeOperations == 1 and not native.readerFinished)
	h.advance(120)
	assert(native.readerFinished and s.activeOperations == 0 and import.activeOperations == 0)
	assert(h.api.getTransactionState({transactionId=s.transactionId}).state == "rolledBack", tostring(s.rollbackFailed))
	h.begin("unblocked")
	assert(not pcall(h.api.withRequestLease,"new-daemon",false,h.api.finishBinaryImport,{importId="import",nativePhase="invoke"}))
end

do
	local h=harness(); local s,native,import=prepared(h,"stale-invoke")
	h.changeOwnershipGeneration(8)
	assert(not pcall(h.api.withRequestLease,"owner",false,h.api.finishBinaryImport,{importId="import",nativePhase="invoke"}))
	assert(not native.invocationAuthorized and s.activeOperations==1 and import.activeOperations==1)
	h.advance(120)
	assert(native.readerFinished and not native.invocationAuthorized and s.activeOperations == 0 and import.activeOperations == 0)
	assert(h.api.getTransactionState({transactionId=s.transactionId}).state == "abandoned")
	h.begin("after-revoked-prepare")
end

-- Generation retirement must not release an authorized native write, including
-- an outstanding native root setter whose old-generation watchdog cannot run.
for _, kind in { "insertion", "root" } do
	local h = harness()
	local s, native, import
	if kind == "insertion" then
		s, native, import = prepared(h, "invalidated-native")
		h.api.withRequestLease("owner", false, h.api.finishBinaryImport, { importId = "import", nativePhase = "invoke" })
	else
		s = h.begin("invalidated-native")
		s.pendingNativeRootWrite = {}
		h.r.beginOp(s)
	end
	h.changeOwnershipGeneration(9)
	local ok, detail = pcall(h.begin, "must-stay-blocked")
	assert(not ok and tostring(detail):find("Restart Studio", 1, true))
	h.advance(1000000)
	assert(h.r.sessions[s.transactionId] == s and not h.r.recoveries[s.transactionId] and s.activeOperations == 1)
	local result = h.api.getTransactionState({ transactionId = s.transactionId })
	assert(result.state == "nativeOutcomeUnknown" and result.nativeOperation.reason:find("Restart Studio", 1, true))
	if import then assert(h.r.imports.import == import and import.activeOperations == 1 and not native.readerFinished) end
end

-- After invoke, daemon loss is NOT evidence of native completion. Only a
-- verified receipt delivered by the current Studio owner (new lease) releases
-- the operation. Invalid receipts and revoked owners cannot release it.
do
	local h = harness(); local s, native, import = prepared(h, "invoked")
	h.api.withRequestLease("owner",false,h.api.finishBinaryImport,{importId="import",nativePhase="invoke"})
	h.api.cancelRequestLease("owner"); h.api.finishRequestLeaseCancellation("owner"); h.api.cleanup()
	h.advance(1000000)
	local state = h.api.getTransactionState({transactionId=s.transactionId})
	assert(state.state == "nativeOutcomeUnknown" and state.nativeOperation.state == "invokedUnknown")
	assert(state.nativeOperation.reason:find("Restart Studio",1,true))
	assert(s.activeOperations == 1 and import.activeOperations == 1 and not native.readerFinished)
	local ok, reason = pcall(h.begin,"blocked")
	assert(not ok and tostring(reason):find("Restart Studio",1,true))
	local params = {importId="import",nativePhase="complete",nativeStatus=4,nativeState=0,nativeCreated=0}
	assert(not pcall(h.api.withRequestLease,"new-daemon",false,h.api.finishBinaryImport,params))
	params.nativeState = 14
	h.owns = false
	assert(not pcall(h.api.withRequestLease,"new-daemon",false,h.api.finishBinaryImport,params))
	assert(not native.readerFinished and s.activeOperations == 1)
	h.owns = true
	local completion = h.api.withRequestLease("new-daemon",false,h.api.finishBinaryImport,params)
	assert(native.readerFinished and s.activeOperations == 0 and import.activeOperations == 0)
	assert(h.api.withRequestLease("new-daemon",false,h.api.finishBinaryImport,params) == completion,
		"replacement daemon must recover its lost completion response")
	assert(not pcall(h.api.withRequestLease,"unrelated-daemon",false,h.api.finishBinaryImport,params))
	h.advance(h.now)
	assert(h.api.getTransactionState({transactionId=s.transactionId}).state == "rolledBack")
end

do
	local h=harness(); local s,native,import=prepared(h,"uninvoked-observation-failure")
	local attempts=0
	h.r.state.rollbackSession=function() attempts+=1; return {} end
	h.ctx.finishNativeImportObservations=function() error("observation unavailable",0) end
	h.advance(120)
	assert(native.readerFinished and s.activeOperations==0 and import.activeOperations==0)
	assert(attempts==0 and s.rollbackFailed and s.finishUninvokedNative)
	h.advance(360)
	assert(h.api.getTransactionState({transactionId=s.transactionId}).state=="abandoned")
	assert(attempts==0 and s.finishUninvokedNative)
	h.ctx.finishNativeImportObservations=function() return {} end
	h.advance(480)
	assert(s.finishUninvokedNative==nil and attempts==0)
end

-- The two previously surviving source mutations must now fail behavior tests.
do
	local mutationRan = false
	local ok = pcall(publicationAfterRevocation, function(source)
		local first = assert(source:find("local function finishTerminalTransaction", 1, true))
		local last = assert(source:find("local function retainTransactionRecovery", first, true))
		local section, count = source:sub(first,last-1):gsub("\t\tctx%.assertSessionOwnership%(session%.ownershipGeneration%)\n", "", 1)
		assert(count == 1); mutationRan = true
		return source:sub(1,first-1) .. section .. source:sub(last)
	end)
	assert(mutationRan and not ok, "publication ownership assertion mutation survived")
	local function pendingMarkerStops(mutate)
		local h = harness(mutate)
		local s = h.begin("pending-marker-mutation")
		s.pendingNativeRootWrite = {}
		h.advance(600)
		assert(#h.delayed == 0, "pending timer guard was removed")
	end
	pendingMarkerStops()
	local pendingMutationRan = false
	local pendingPassed = pcall(pendingMarkerStops, function(source)
		local changed, count = source:gsub("values%[key%] == session and not nativeWorkPending%(session%)", "values[key] == session", 1)
		assert(count == 1); pendingMutationRan = true
		return changed
	end)
	assert(pendingMutationRan and not pendingPassed, "pending-marker re-arm mutation survived")
end

print("Recovery lifecycle: real queue, ownership, cancellation publication, native daemon loss and both review mutations passed")
