local base=require('./transaction-recovery-harness')
for _,duration in {0,30} do
    for _,mode in {'idle','commit','rollback','lease','cleanup'} do
        local h=base.harness(); local s=h.begin(mode)
        local starts,ends={},{}
        h.r.state.rollbackSession=function()
            table.insert(starts,h.now)
            if duration>0 then coroutine.yield('rollback') end
            table.insert(ends,h.now)
            error('restoration failed',0)
        end
        local function execute(callback)
            local thread=coroutine.create(callback)
            local ok,detail=coroutine.resume(thread)
            assert(ok,tostring(detail))
            if coroutine.status(thread)=='suspended' then
                assert((s.activeOperations or 0)>0,'yielding rollback must own session fence')
                h.now+=duration
                local worker=h.gate.threads[#h.gate.threads]
                h.gate.resume(worker)
                ok,detail=true,nil
                assert(ok,tostring(detail))
            end
            assert(coroutine.status(thread)=='dead')
        end
        local function advance(untilTime)
            while true do
                table.sort(h.delayed,function(a,b) return a.at<b.at end)
                if not h.delayed[1] or h.delayed[1].at>untilTime then break end
                local job=table.remove(h.delayed,1)
                h.now=math.max(h.now,job.at)
                execute(job.run)
            end
            h.now=math.max(h.now,untilTime)
        end
        execute(function()
            if mode=='commit' then
                s.cancelRequested=true
                assert(not pcall(h.api.withRequestLease,'owner',false,h.api.commitTransaction,{transactionId=mode}))
            elseif mode=='rollback' then
                assert(not pcall(h.api.withRequestLease,'owner',false,h.api.rollbackTransaction,{transactionId=mode}))
            elseif mode=='lease' then
                h.api.cancelRequestLease('owner');h.api.finishRequestLeaseCancellation('owner')
            elseif mode=='cleanup' then h.api.cleanup() end
        end)
        local expected=if mode=='idle' then {120,240+duration} else {0,120+duration,240+2*duration}
        if mode~='idle' then
            assert(h.api.getTransactionState({transactionId=mode}).state=='rollbackFailed')
            local old=s.updatedAt
            advance(60);h.api.getTransactionState({transactionId=mode});assert(s.updatedAt==old)
        end
        advance(expected[#expected]+duration)
        assert(h.api.getTransactionState({transactionId=mode}).state=='abandoned')
        assert(#starts==#expected)
        for index,start in expected do assert(starts[index]==start and ends[index]==start+duration) end
        print('TIMING duration='..duration,mode,'starts='..table.concat(starts,','),'returns='..table.concat(ends,','),'abandoned='..ends[#ends])
    end
end

for _,via in {'timer','prune'} do
    local h=base.harness(); local s=h.begin(via)
    local attempts=0
    h.r.state.rollbackSession=function() attempts+=1;error('failed',0) end
    if via=='timer' then h.advance(119.999) else h.now=119.999;assert(not pcall(h.begin,'next')) end
    assert(attempts==0)
    if via=='timer' then h.advance(120) else h.now=120;assert(not pcall(h.begin,'next')) end
    assert(attempts==1 and s.idleRollbackFailures==1)
    local ok,detail=pcall(h.begin,'next')
    assert(not ok and tostring(detail):find('failed rollback retained for recovery',1,true))
    h.now=180;h.api.getTransactionState({transactionId=via});assert(s.updatedAt==120)
    if via=='timer' then h.advance(240) else h.now=240;h.begin('next') end
    assert(attempts==2 and h.api.getTransactionState({transactionId=via}).state=='abandoned')
    print('BOUNDARY',via,'119.999 none; 120 failure1; 240 abandoned; failed polls/refusals did not renew')
end

do
    local h=base.harness();local s=h.begin('healthy')
    local ok,detail=pcall(h.begin,'other')
    assert(not ok and tostring(detail):find('wait for its current operation',1,true))
    h.advance(60);h.api.getTransactionState({transactionId='healthy'})
    h.advance(120);assert(h.r.sessions.healthy==s)
    h.advance(180);assert(h.api.getTransactionState({transactionId='healthy'}).state=='rolledBack')
    print('HEALTHY status poll at60 extends deadline to180, refusal does not diagnose failure')
end
print('Recovery timing table and inclusive boundaries passed with the real exclusive queue and ownership predicate')
