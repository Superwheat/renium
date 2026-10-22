local base = require("./transaction-recovery-harness")

-- Complete the history boundary, then lose ownership before publication. Each
-- public exit must publish that result without entering structural work again.
for _, changedGeneration in { false, true } do
for _, terminal in { "committed", "rolledBack" } do
	for _, retry in { "timer", "prune", "commit", "rollback", "cleanup", "cancellation" } do
		local h = base.harness()
		local s = h.begin(`{terminal}-{retry}`)
		local rollbacks = 0
		h.r.state.rollbackSession = function()
			rollbacks += 1
			h.owns = false
			return {}
		end
		if terminal == "committed" then
			s.commitPrepared = true
			local history = h.game:GetService("ChangeHistoryService")
			local finish = history.FinishRecording
			history.FinishRecording = function(...)
				finish(...)
				h.owns = false
			end
		end
		local operation = if terminal == "committed" then h.api.commitTransaction else h.api.rollbackTransaction
		assert(not pcall(h.api.withRequestLease, "owner", false, operation, { transactionId = s.transactionId }))
		assert(s.pendingTerminal.state == terminal and s.onExpire and s.activeOperations == 0)
		local response = s.pendingTerminal.response
		response.publicationMarker = "original result"
		local historyFinishes = #h.history
		h.owns = true
		if changedGeneration then h.changeOwnershipGeneration(9) end
		if retry == "timer" then h.advance(120)
		elseif retry == "prune" then
			h.now = 120
			h.gate.call(function() h.r.prune(h.r.sessions) end)
		elseif retry == "cleanup" then h.api.cleanup()
		elseif retry == "cancellation" then
			h.api.cancelRequestLease("owner")
			local result = h.api.finishRequestLeaseCancellation("owner")
			assert(result.rolledBack == (if terminal == "rolledBack" then 1 else 0))
		else
			h.api.withRequestLease("owner", false, if retry == "commit" then h.api.commitTransaction else h.api.rollbackTransaction,
				{ transactionId = s.transactionId })
		end
		local result = h.api.getTransactionState({ transactionId = s.transactionId })
		assert(result.state == terminal and result.publicationMarker == "original result", `{terminal}/{retry}`)
		assert(rollbacks == (if terminal == "rolledBack" then 1 else 0), "completed structural work was replayed")
		assert(#h.history == historyFinishes and not h.r.sessions[s.transactionId])
		assert(not s.pendingTerminal and not s.onExpire and s.activeOperations == 0 and not h.gate.busy())
	end
end
end

-- Even a failure between the commit fence and constructing pendingTerminal
-- cannot authorize rollback. The lower-level rollback entry refuses it too.
do
	local h = base.harness()
	local s = h.begin("commit-fence")
	s.commitFence = true
	local ok, detail = pcall(h.api.withRequestLease, "owner", false, h.r.state.rollbackSession, s, h.ctx)
	assert(not ok and tostring(detail):find("cannot enter structural rollback", 1, true))
	assert(h.api.withRequestLease("owner", false, h.api.rollbackTransaction, { transactionId = s.transactionId }).committed)
end

for _, trigger in { "begin", "timer", "release-gap" } do
	local h = base.harness(nil, true)
	local s = h.begin(`reacquire-{trigger}`)
	local retainedRecord = { marker = "outside edit" }
	table.insert(h.journals.state.changeJournal.records, retainedRecord)
	local rollbacks = 0
	h.r.state.rollbackSession = function()
		rollbacks += 1
		error("one shutdown rollback failure", 0)
	end
	h.api.cleanup()
	assert(s.rollbackFailed and rollbacks == 1 and s.activeOperations == 0)
	h.changeOwnershipGeneration(8)
	h.owns = false
	if trigger == "release-gap" then h.advance(120) end
	h.changeOwnershipGeneration(9)
	h.owns = true
	if trigger == "begin" then h.begin("next")
	else
		local deadline = h.now + 120
		h.advance(deadline - 0.001)
		assert(h.r.sessions[s.transactionId] == s)
		h.advance(deadline)
		h.begin("next")
	end
	local result = h.api.getTransactionState({ transactionId = s.transactionId })
	assert(result.state == "abandoned" and result.recovery.retryStopped)
	assert(result.retirementReason:find("generation 7 to 9", 1, true))
	assert(result.recovery.ownershipGeneration == 7 and result.recovery.retiredUnderGeneration == 9)
	assert(result.recovery.expiresAtUnix == h.now + 1800 and rollbacks == 1)
	assert(s.ownershipGeneration == 7 and h.r.recoveries[s.transactionId].session == s)
	assert(h.journals.state.changeJournal.id == "next" and h.journals.retiring[s.transactionId])
	assert(s.changeJournal[1] == retainedRecord and result.recovery.unreplayedEdits == 1)
	-- Stop the newer fixture, then prove old scene work never resumes, even at
	-- the retention deadline and long after the diagnostic has been published.
	h.r.state.rollbackSession = function(nextSession)
		h.ctx.finishStudioChangeJournal(nextSession.transactionId)
		nextSession.journalActive = false
		return {}
	end
	h.api.withRequestLease("owner", false, h.api.rollbackTransaction, { transactionId = "next" })
	h.advance(h.now + 1800)
	assert(not h.r.recoveries[s.transactionId] and rollbacks == 1)
	assert(not h.journals.retiring[s.transactionId] and not s.snapshot and not s.withOwnership)
	assert(h.api.getTransactionState({ transactionId = s.transactionId }).recovery.state == "expired")
end

-- A new owner must wait for an actual operation to leave its fence before
-- locally retiring the old transaction. It never runs the old rollback.
do
	local h = base.harness(nil, true)
	local s = h.begin("running-old-operation")
	h.r.beginOp(s)
	h.changeOwnershipGeneration(9)
	assert(not pcall(h.begin, "too-early") and h.r.sessions[s.transactionId] == s)
	h.r.endOp(h.r.sessions, s.transactionId, s)
	h.begin("after-operation")
	assert(h.api.getTransactionState({ transactionId = s.transactionId }).state == "abandoned")
end

-- A completed result also survives ownership release/reacquisition. Its old
-- cleanup is retained, but the terminal state cannot become abandonment.
for _, terminal in { "committed", "rolledBack" } do
	local h = base.harness(nil, true)
	local s = h.begin(`pending-reacquire-{terminal}`)
	s.pendingTerminal = { state = terminal, response = { marker = "completed" } }
	s.commitFence = terminal == "committed"
	h.r.state.rollbackSession = function() error("must not replay", 0) end
	h.changeOwnershipGeneration(9)
	h.advance(120)
	local result = h.api.getTransactionState({ transactionId = s.transactionId })
	assert(result.state == terminal and result.marker == "completed" and result.recovery.retryStopped)
	h.begin("after-pending")
end

local function importParams(transactionId, importId)
	return {
		importId = importId, transactionId = transactionId, totalBytes = 1024, totalChunks = 1, instanceCount = 0,
		externalReferencesPostApplied = true,
		groups = {{ service = "Workspace", targetPath = { "Workspace" }, payloadRootName = "Workspace", rootPaths = {}, count = 0 }},
	}
end

-- Review retention reproduction: real import allocator, 1 MiB retained
-- snapshot, and a lost owner during terminal expectation release.
for _, finished in { false, true } do
	local h = base.harness()
	local s = h.begin("import-retention")
	local imports = {}
	for index = 1, 4 do
		local id = `retained-{index}`
		assert(h.api.withRequestLease("owner", false, h.api.beginBinaryImport, importParams(s.transactionId, id)).ok)
		local import = h.r.imports[id]
		imports[index] = import
		if finished then import.nativeHeld = { readerArmed = true, readerFinished = true } end
	end
	assert(not pcall(h.api.withRequestLease, "owner", false, h.api.beginBinaryImport, importParams(s.transactionId, "fifth")))
	s.snapshot.reviewPayload = buffer.create(1024 * 1024)
	h.ctx.finishEditorTransactionExpectation = function() h.owns = false end
	assert(h.api.withRequestLease("owner", false, h.api.rollbackTransaction, { transactionId = s.transactionId }).rolledBack)
	h.changeOwnershipGeneration(9)
	h.owns = true
	h.advance(1799.999)
	assert(h.r.imports["retained-1"] == imports[1] and h.r.recoveries[s.transactionId])
	h.advance(1800)
	assert(next(h.r.imports) == nil and not h.r.recoveries[s.transactionId])
	assert(not s.snapshot and not s.withOwnership and not s.scheduleExpiry)
	for _, import in imports do
		assert(next(import) == nil, "eviction must release timer-held import payloads and transaction closures")
	end
	assert(h.api.getTransactionState({ transactionId = s.transactionId }).recovery.state == "expired")
	h.ctx.finishEditorTransactionExpectation = function() end
	h.owns = true
	h.begin("after-import-expiry")
	for index = 1, 4 do
		assert(h.api.withRequestLease("owner", false, h.api.beginBinaryImport, importParams("after-import-expiry", `new-{index}`)).ok)
	end
	-- Exercise the real aggregate-byte check without allocating 512 MiB. The
	-- capacity accounting uses totalBytes, independently of payload allocation.
	h.r.imports["new-1"].totalBytes = 536870912 - 2048
	local ok, detail = pcall(h.api.withRequestLease, "owner", false, h.api.beginBinaryImport, importParams("after-import-expiry", "new-4"))
	assert(not ok and tostring(detail):find("aggregate buffered-byte limit", 1, true))
	h.r.imports["new-1"].totalBytes -= 1024
	assert(h.api.withRequestLease("owner", false, h.api.beginBinaryImport, importParams("after-import-expiry", "new-4")).ok)
end

-- A delayed retention callback belongs to a particular terminal transfer, not
-- an id that may be reused after the ordinary 300-second outcome TTL.
do
	local h = base.harness()
	local original = h.begin("reused-id")
	original.commitPrepared = true
	h.api.withRequestLease("owner", false, h.api.commitTransaction, { transactionId = "reused-id" })
	h.advance(301)
	local replacement = h.begin("reused-id")
	h.r.state.rollbackSession = function() error("retained failure", 0) end
	h.advance(541)
	assert(h.r.recoveries["reused-id"].session == replacement)
	h.advance(1800)
	assert(h.r.recoveries["reused-id"].session == replacement, "old timer evicted a newer recovery")
	h.advance(2341)
	assert(not h.r.recoveries["reused-id"])
end

-- Reusing an import id must not let old eviction clear the new transaction's
-- registry slot, payload, callbacks or tracker observers.
do
	local h = base.harness(nil, true)
	local old = h.begin("retained-import-owner")
	h.api.withRequestLease("owner", false, h.api.beginBinaryImport, importParams(old.transactionId, "same-import-id"))
	h.ctx.finishEditorTransactionExpectation = function() h.owns = false end
	h.api.withRequestLease("owner", false, h.api.rollbackTransaction, { transactionId = old.transactionId })
	assert(h.r.imports["same-import-id"].transaction == old)
	h.ctx.finishEditorTransactionExpectation = function() end
	h.changeOwnershipGeneration(9)
	h.owns = true
	local newer = h.begin("new-import-owner")
	h.api.withRequestLease("owner", false, h.api.beginBinaryImport, importParams(newer.transactionId, "same-import-id"))
	local import = h.r.imports["same-import-id"]
	-- Hold genuine operation fences while eviction runs, so ordinary idle
	-- expiry cannot remove the new transaction or import first.
	h.r.beginOp(newer)
	h.r.beginOp(import)
	h.advance(1800)
	assert(not h.r.recoveries[old.transactionId] and h.r.imports["same-import-id"] == import)
	assert(import.transaction == newer and import.payload and import.withOwnership == newer.withOwnership)
	assert(h.journals.state.changeJournal.id == newer.transactionId)
end

-- Preserve review 26's count/TTL limits and healthy history choice alongside
-- the new retirement paths, using its deterministic timing conditions.
do
	local h = base.harness()
	h.r.state.rollbackSession = function() error("failed", 0) end
	for index = 1, 65 do
		h.begin(`diagnostic-{index}`)
		h.advance(index * 240)
	end
	h.advance(h.now + 1800)
	local count = 0
	for _ in h.r.diagnostics do count += 1 end
	assert(count == 64 and next(h.r.recoveries) == nil)
	assert(h.api.getTransactionState({ transactionId = "diagnostic-1" }).state == "notFound")
	assert(h.api.getTransactionState({ transactionId = "diagnostic-65" }).recovery.state == "expired")
end

do
	local h = base.harness()
	local s = h.begin("blocked-opportunities")
	h.r.state.rollbackSession = function() error("failed", 0) end
	h.advance(240)
	h.begin("newer")
	for time = 300, 1200, 60 do
		h.advance(time)
		h.api.getTransactionState({ transactionId = "newer" })
	end
	assert(s.recoveryTicks == 8 and s.recoveryAttempts == 1 and s.recoveryStopped)
	h.advance(2040)
	assert(not h.r.recoveries[s.transactionId])
end

do
	local h = base.harness()
	local s = h.begin("healthy-ttl")
	s.commitPrepared = true
	h.api.withRequestLease("owner", false, h.api.commitTransaction, { transactionId = s.transactionId })
	assert(#h.history == 2 and h.history[1].operation == h.history[2].operation)
	h.advance(299.999)
	assert(h.api.getTransactionState({ transactionId = s.transactionId }).state == "committed")
	h.advance(300)
	assert(h.api.getTransactionState({ transactionId = s.transactionId }).state == "notFound")
end

print("Recovery retirement: terminal exits, generation 7 -> 9 handoff, import capacity, 64 diagnostics, eight opportunities and outcome TTL passed")
