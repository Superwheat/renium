local base = require("./transaction-recovery-harness")
for _, previous in {true, false} do
	local h=base.harness(if previous then function(source)
		local first=assert(source:find("local function retireRoot",1,true))
		local last=assert(source:find("for groupIndex",first,true))
		local section,count=source:sub(first,last-1):gsub("instance:Destroy%(%)", "TransactionState.destroyOwned(instance, ctx)",1)
		assert(count==1)
		return source:sub(1,first-1)..section..source:sub(last)
	end else nil)
	local s=h.begin("large-commit")
	local root=base.outsideNode(h,"DetachedOutgoing",nil)
	for index=1,10000 do root.children[index]=base.outsideNode(h,"Child"..index,root) end
	local expectations=0
	h.ctx.expectParentChange=function() expectations+=1 end
	h.ctx.expectPropertyEvent=function() expectations+=1 end
	s.nativeUndo={prepared={{outgoing={root}}},replacements={}}
	s.commitPrepared=true
	local result=h.api.withRequestLease("owner",false,h.api.commitTransaction,{transactionId=s.transactionId})
	assert(result.state=="committed" and root.destroyed and not h.r.recoveries[s.transactionId])
	assert(expectations==(if previous then 10001 else 0))
end
print("Healthy commit (10,000 descendants): retiring a root registers no expectation; registering one per descendant is detected")
