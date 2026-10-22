local base = require("./transaction-recovery-harness")
local harness, outsideNode = base.harness, base.outsideNode

-- Native roots have independent pending ownership. A later transaction that
-- fills the original target conflicts; successful entries never resurrect.
for _, newerContents in { false, true } do
	local h = harness(); local s = h.begin("native-roots")
	local parent = outsideNode(h,"Package",nil)
	local holding = outsideNode(h,"Holding",h.realGame:GetService("CoreGui"))
	local root = outsideNode(h,"Car",holding); holding.children = {root}
	s.nativeUndo = {retainedParent=holding,replacements={},prepared={{target=parent,outgoing={root}}}}
	h.r.state.rollbackSession = function() error("missing parent",0) end
	h.advance(240)
	assert(s.pendingNativeRecovery[root] and root.Parent == holding)
	local nextSession = h.begin("newer")
	parent.Parent = h.realGame:GetService("Workspace")
	local fresh
	if newerContents then
		fresh = outsideNode(h,"Car",parent)
		parent.children = {fresh}
	end
	h.advance(300); h.api.getTransactionState({transactionId="newer"}); h.advance(360)
	assert(root.Parent == holding)
	nextSession.commitPrepared = true
	h.api.withRequestLease("owner",false,h.api.commitTransaction,{transactionId="newer"})
	h.advance(480)
	if newerContents then
		assert(root.Parent == holding and fresh.Parent == parent and s.pendingNativeRecovery[root])
		local state = h.api.getTransactionState({transactionId=s.transactionId})
		assert(state.recovery.unresolvedNativeRoots == 1)
		assert(s.cleanupErrors["recovery native root " .. tostring(root)].error:find("newer contents",1,true))
	else
		assert(root.Parent == parent and not s.pendingNativeRecovery[root])
		local state = h.api.getTransactionState({transactionId=s.transactionId})
		assert(state.recovery.unresolvedNativeRoots == 0)
		root.Parent = nil -- intentional user removal after verified recovery
		local later = outsideNode(h,"Car",parent); parent.children = {later}
		h.advance(600)
		assert(root.Parent == nil and later.Parent == parent and not s.pendingNativeRecovery[root])
	end
end

-- A tracked value edit in the target service also prevents stale native replay,
-- even when its direct child identities did not change.
for _, change in {"outside", "native", "attribute", "observationLost"} do
	local h = harness(); local s = h.begin("native-values")
	local parent = outsideNode(h,"Parent",nil)
	local root = outsideNode(h,"Car",nil)
	s.nativeUndo = {replacements={},prepared={{target=parent,outgoing={root}}}}
	h.r.state.rollbackSession = function() error("missing",0) end
	h.advance(240)
	parent.Parent = h.realGame:GetService("Workspace")
	if change == "native" then h.exportGeneration=1
	elseif change == "attribute" then h.attributeObservation.generation=1
	elseif change == "observationLost" then h.attributeObservation.active=false
	else h.ctx.studioChangeGeneration = function() return 1 end end
	h.advance(360)
	assert(root.Parent == nil and s.pendingNativeRecovery[root])
end

-- Completing one native root cannot keep it eligible because another root's
-- setter failed. Each root retires only after its own verified attachment.
do
	local h=harness(); local s=h.begin("partial-roots")
	local parent=outsideNode(h,"Parent",nil)
	local first=outsideNode(h,"First",nil)
	local second=outsideNode(h,"Second",nil); second.failParent=true
	s.nativeUndo={replacements={},prepared={{target=parent,outgoing={first,second}}}}
	h.r.state.rollbackSession=function() error("missing",0) end
	h.advance(240); parent.Parent=h.realGame:GetService("Workspace")
	h.parentWrite = function(instance, target)
		if target == parent and (instance == first or instance == second) then
			h.exportGeneration = (h.exportGeneration or 0) + 1
		end
	end
	h.advance(360)
	assert(first.Parent==parent and not s.pendingNativeRecovery[first] and s.pendingNativeRecovery[second])
	second.failParent=false; h.advance(480)
	assert(second.Parent==parent and next(s.pendingNativeRecovery)==nil)
	first.Parent=nil; second.Parent=nil; h.advance(600)
	assert(first.Parent==nil and second.Parent==nil)
end

-- Retain the original detached journal identity, reject a path lookalike, and
-- replay once alongside a newer successful transaction's unrelated content.
do
	local h = harness(); local s = h.begin("outside-root")
	local parent = outsideNode(h,"Package",h.realGame:GetService("Workspace"))
	local root = outsideNode(h,"Manual",parent); parent.children = {root}
	s.mutated = true; s.snapshot.groups = {{target=parent,preserved={}}}
	s.changeJournal = {{instance=root,currentInstance=root,parent=parent,service="Workspace",structural=true,properties={},attributes={},parentPathSegments={"Workspace","Package"}}}
	h.r.state.rollback = function() parent.Parent=nil; error("missing",0) end
	h.advance(240)
	outsideNode(h,"Package",h.realGame:GetService("Workspace"))
	h.advance(360); assert(root.Parent == nil and #s.changeJournal == 1)
	local newer = h.begin("newer-outside")
	parent.Parent = h.realGame:GetService("Workspace")
	local newContent = outsideNode(h,"NewerContent",parent); parent.children={root,newContent}
	h.advance(420); h.api.getTransactionState({transactionId="newer-outside"}); h.advance(480)
	assert(root.Parent == nil)
	newer.commitPrepared=true
	h.api.withRequestLease("owner",false,h.api.commitTransaction,{transactionId="newer-outside"})
	h.advance(600)
	assert(root.Parent == parent and newContent.Parent == parent and #s.changeJournal == 0)
	root.Parent=nil; h.advance(840)
	assert(root.Parent == nil and #s.changeJournal == 0)
end

-- Real placement and value replay with a single engine-boundary setter failure.
-- Each verified field advances only its own guard; a new external edit remains
-- a conflict rather than being adopted by an indiscriminate guard refresh.
for _, kind in {"property", "attribute", "tag"} do
	for _, outsideEdit in {false, true} do
		local h=harness(); local s=h.begin("partial-"..kind)
		local parent=outsideNode(h,"Parent",h.realGame:GetService("Workspace"))
		local root=outsideNode(h,"Manual",nil)
		root.ValueA="old"; root.ValueB="old"
		root.attributes={A="old",B="old"}; root.tags={"OldA","OldB"}
		local record={instance=root,currentInstance=root,parent=parent,service="Workspace",structural=true,
			properties={},attributes={}}
		if kind=="property" then record.properties={ValueA={captured=true,value="outside"},ValueB={captured=true,value="outside"}}
		elseif kind=="attribute" then record.attributesSnapshot={A="outside",B="outside"}
		else record.tags={"NewA","NewB"} end
		s.changeJournal={record}; s.preservedJournalRoots={[root]=true}
		h.r.state.rollbackSession=function() error("structural failed",0) end
		h.modules.RbxDom.readProperty=function(instance,name) return true,instance[name] end
		h.ctx.expectPropertyEvent=function() end
		h.ctx.expectAttributeEvent=function() end
		h.ctx.expectTagChange=function() end
		local writes=0
		local function write()
			writes+=1
			if writes==2 then error("transient setter failure",0) end
		end
		if kind=="property" then
			local mt=getmetatable(root); local setter=mt.__newindex
			mt.__newindex=function(instance,name,value)
				if name=="ValueA" or name=="ValueB" then write() end
				setter(instance,name,value)
			end
		elseif kind=="attribute" then h.attributeWrite=write
		else h.tagWrite=write end
		h.advance(240)
		assert(writes==2 and root.Parent==parent and #s.changeJournal==1, `partial {kind} did not reach second write`)
		if outsideEdit then
			if kind=="property" then
				local values=getmetatable(root).__index
				values[if root.ValueA=="outside" then "ValueA" else "ValueB"]="newer"
			elseif kind=="attribute" then
				root.attributes[if root.attributes.A=="outside" then "A" else "B"]="newer"
			else table.insert(root.tags,"Newer") end
		end
		h.advance(360)
		if outsideEdit then
			assert(#s.changeJournal==1 and writes==2, `partial {kind} overwrote newer edits`)
		else
			assert(#s.changeJournal==0 and writes>2, `partial {kind} failed to resume: {s.cleanupErrors["recovery journal "..tostring(root)].error}`)
			if kind=="property" then assert(root.ValueA=="outside" and root.ValueB=="outside")
			elseif kind=="attribute" then assert(root.attributes.A=="outside" and root.attributes.B=="outside")
			else table.sort(root.tags); assert(table.concat(root.tags,",")=="NewA,NewB") end
		end
	end
end

-- Eight attempts maximum, then bounded retention with count-bearing diagnostics.
do
	local h=harness(); local s=h.begin("bounded")
	local parent=outsideNode(h,"Missing",nil); local root=outsideNode(h,"Unsaved",nil)
	s.changeJournal={{instance=root,currentInstance=root,parent=parent,service="Workspace",properties={},attributes={}}}
	s.preservedJournalRoots={[root]=true}
	h.r.state.rollbackSession=function() error("failed",0) end
	h.advance(240)
	local summary=h.api.getTransactionState({transactionId="bounded"}).recovery
	assert(summary.lifetime=="pluginSession" and summary.retentionSeconds==1800 and summary.retryLimit==8)
	assert(summary.unreplayedEdits==1 and summary.detachedRoots==1 and summary.lossWarning:find("Studio close",1,true))
	h.advance(1800)
	assert(s.recoveryAttempts==8 and s.recoveryStopped and h.r.recoveries.bounded)
	h.advance(2040)
	assert(h.r.recoveries.bounded==nil and root.Parent==nil and not root.destroyed)
	local expired=h.api.getTransactionState({transactionId="bounded"})
	assert(expired.state=="abandoned" and expired.recovery.state=="expired" and expired.recovery.recoveryDataMayBeLost)
	assert(expired.recovery.unreplayedEdits==1 and expired.recovery.detachedRoots==1)
	assert(#h.delayed==0 and s.recoveryAttempts==8)
	local fresh=harness()
	fresh.ctx.runtimeId="reloaded-plugin-session"
	local lost=fresh.api.getTransactionState({transactionId="bounded"})
	assert(lost.state=="notFound" and lost.recoveryDataMayBeLost and lost.pluginSessionId~=summary.pluginSessionId)
end

-- UpdateSourceAsync can suspend before invoking its write callback. Validate
-- ownership and the saved value again inside that callback and its fallback.
for _, revoked in {true, false} do
	local h=harness(); local s=h.begin("source-yield")
	local parent=outsideNode(h,"Parent",h.realGame:GetService("Workspace"))
	local root=outsideNode(h,"Script",parent); root.Source="old"
	function root:IsA(class) return class=="LuaSourceContainer" end
	s.changeJournal={{instance=root,currentInstance=root,parent=root.Parent,service="Workspace",
		properties={Source={captured=true,value="outside"}},attributes={}}}
	h.r.state.rollbackSession=function() error("failed",0) end
	h.ctx.expectPropertyEvent=function() end
	h.ctx.cancelExpectedEvent=function() end
	h.scriptEditor.UpdateSourceAsync=function(_, instance, callback)
		coroutine.yield()
		instance.Source=callback()
	end
	h.advance(120)
	h.now=240; table.remove(h.delayed,1).run()
	local waiter=h.gate.spawn(table.remove(h.delayed,1).run)
	local worker=h.gate.threads[#h.gate.threads]
	assert(h.gate.busy() and root.Source=="old", tostring(s.cleanupErrors and s.cleanupErrors["recovery journal "..tostring(root)] and s.cleanupErrors["recovery journal "..tostring(root)].error))
	if revoked then h.owns=false else root.Source="newer" end
	h.gate.resume(worker)
	assert(root.Source==(if revoked then "old" else "newer") and #s.changeJournal==1)
	assert(not h.gate.busy() and h.getGeneration()==nil and coroutine.status(waiter)=="dead")
end

print("Recovery replay: native conflicts/retirement, outside identity, partial values, yielding source guards and bounded lifetime passed")
