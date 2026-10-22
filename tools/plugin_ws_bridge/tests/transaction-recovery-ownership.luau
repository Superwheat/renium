-- A plain source read (getLiveSourceBatch during push verification) runs
-- outside the exclusive queue, so no generation is bound. It must pass while
-- the plugin owns the session lock and fail only once ownership is lost.
local base = require("./transaction-recovery-harness")
local h = base.harness()
assert(h.getGeneration() == nil, "no exclusive generation is bound outside a request")
h.ctx.assertSessionOwnership()
h.owns = false
assert(not pcall(h.ctx.assertSessionOwnership), "a lost lock still fails outside a request")
h.owns = true
-- An explicit generation that disagrees with the bound one is still rejected.
h.gate.call(function()
	h.ctx.assertSessionOwnership(h.getGeneration())
	assert(not pcall(h.ctx.assertSessionOwnership, (h.getGeneration() or 0) + 1))
end)
print("Ownership assertion: unbound reads pass while the lock is owned; lost locks and foreign generations still fail")
