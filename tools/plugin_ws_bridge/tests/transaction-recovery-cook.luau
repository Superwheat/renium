-- A finished native MeshId write asks the plugin to cook the mesh; Studio
-- ownership can change while CreateMeshPartAsync yields. The cook must not
-- apply a donor built under a revoked owner, must destroy that donor, and
-- must release its operation fence so a new transaction can begin.
local base = require("./transaction-recovery-harness")
local roblox = require("@lune/roblox")
local enums = setmetatable({ ContentSourceType = { Uri = "Uri" } }, { __index = function(_, key) return roblox.Enum[key] end })
local h = base.harness(nil, nil, enums)
local s = h.begin("mesh-cook")
h.ctx.expectPropertyEvent = function() return nil end
local mesh = base.outsideNode(h, "Mesh", h.realGame:GetService("Workspace"))
mesh.ClassName = "MeshPart"
mesh.MeshSize = roblox.Vector3.zero
mesh.MeshContent = { SourceType = enums.ContentSourceType.Uri, Uri = "fixture-asset" }
mesh.IsA = function(_, name) return name == "MeshPart" or name == "BasePart" end
mesh.Size = roblox.Vector3.one
local applied, destroyed, asyncThread = 0, 0, nil
mesh.ApplyMesh = function() applied += 1 end
local donor = { Destroy = function() destroyed += 1 end }
local originalGetService = h.game.GetService
h.game.GetService = function(self, name)
	if name == "AssetService" then
		return { CreateMeshPartAsync = function()
			asyncThread = coroutine.running()
			coroutine.yield("asset-service-engine-yield")
			return donor
		end }
	end
	return originalGetService(self, name)
end
h.r.setResolver(function() return mesh end)
s.nativeRootWrites = {{ instance = mesh, name = "MeshId", finished = true,
	change = { service = "Workspace", pathSegments = {"Workspace", "Mesh"} } }}
local ok, detail
h.gate.spawn(function()
	ok, detail = pcall(h.api.withRequestLease, "owner", false, h.api.getTransactionState,
		{ transactionId = s.transactionId, nativeRootWrite = 1, cookNativeRootWrite = true })
end)
assert(asyncThread and h.gate.busy() and s.activeOperations == 1)
h.changeOwnershipGeneration(9)
h.gate.resume(asyncThread)
assert(ok == false and tostring(detail):find("ownership was lost", 1, true), tostring(detail))
assert(not h.gate.busy() and coroutine.status(asyncThread) == "dead")
assert(s.pendingNativeRootWrite == nil and s.nativeServiceImport == nil)
assert(s.activeOperations == 0, "the cook releases its operation fence when it throws")
assert(applied == 0, "a donor built under a revoked owner is never applied")
assert(destroyed == 1, "the donor is destroyed even when restoration throws")
local nextResult = h.api.withRequestLease("fresh-owner", false, h.api.beginTransaction,
	{ transactionId = "next-healthy", services = {"Workspace"} })
assert(nextResult.state == "open" and h.now == 0, "a new transaction begins at once under the new owner")
assert(h.api.getTransactionState({ transactionId = s.transactionId }).state == "abandoned")
print("Mesh cook: revoked ownership releases the fence, discards the donor and lets the next transaction begin")
