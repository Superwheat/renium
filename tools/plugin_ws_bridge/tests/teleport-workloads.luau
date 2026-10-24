local fs = require("@lune/fs")
local luau = require("@lune/luau")
local serde = require("@lune/serde")
local base64 = require("./base64")
for _, vector in { { "", "" }, { "f", "Zg==" }, { "fo", "Zm8=" }, { "foo", "Zm9v" }, { "\0\255\128", "AP+A" } } do
	assert(base64.encode(vector[1]) == vector[2])
	assert(base64.decode(vector[2]) == vector[1])
end
local State = require("../BridgeTeleportState.module")

local now = 0
local queue = State.create(function() return now end)
queue.observe(12, "Player1", {}, false)
assert(queue.pending == nil)
queue.arm()
queue.arm()
assert(queue.phase == "idle" and queue.id == 0, "an idle server can rearm after daemon replacement")
queue.observe(12, "Player1", { teleportData = { level = 2 } }, false)
assert(not pcall(queue.arm), "rearming must not discard an in-flight request")
queue.observe(12, "Player2", {}, false)
queue.observe(12, "Player1", { teleportData = { level = 3 } }, false)
assert(queue.poll() == nil)
now = 1
local attempt = queue.poll()
assert(#attempt.players == 2 and attempt.data.Player1.teleportData.level == 3)
queue.observe(13, "Player3", {}, false)
assert(#attempt.players == 2, "in-flight attempts cannot collect new requests")
assert(not pcall(queue.advance, 0, "loading", "uploading"))
assert(queue.fail(0) == false and queue.phase == "loading")
assert(queue.fail(1) == false and queue.phase == "idle")
queue.observe(12, "Player1", {}, false)
queue.observe(13, "Player2", {}, true)
assert(queue.pending.error and queue.pending.reserved)
queue.fail(queue.id)
queue.observe(99, "Player1", {}, false)
assert(queue.pending.error == nil, "destinations are resolved by the daemon, not an allowlist")
queue.fail(queue.id)
for _, place in { 0, -1, 1.5, 9007199254740992 } do
	queue.observe(place, "Player1", {}, false)
	assert(queue.pending.error == "Invalid destination ID")
	queue.fail(queue.id)
end
queue.observe(12, "Player1", {}, false)
now += 1
queue.poll()
for _, phases in { { "loading", "uploading" }, { "uploading", "prepared" }, { "prepared", "committing" } } do
	queue.advance(queue.id, phases[1], phases[2])
end
assert(queue.fail(queue.id) == true and queue.phase == "failed")
queue.observe(12, "Player1", {}, false)
assert(queue.pending == nil, "a partially committed world requires Stop Play")
local completed = State.create(function() return now end)
completed.arm()
completed.observe(12, "Player1", {}, false)
now += 1
completed.poll()
completed.advance(completed.id, "loading", "prepared")
completed.advance(completed.id, "prepared", "committing")
completed.advance(completed.id, "committing", "arriving")
completed.complete(completed.id)
assert(completed.fail(completed.id), "an uncertain completion cannot claim the source was retained")

local services, request, destroyed, preparedCount, committed, arrivals, resumed = {}, nil, 0, 0, 0, 0, 0
local nativeRelay
services.ServerStorage = {}
local isEdit, yieldDeserialize, yieldArrival = false, false, false
services.RunService = { IsStudio = function() return true end, IsEdit = function() return isEdit end }
services.HttpService = { JSONEncode = function(_, value) return serde.encode("json", value) end,
	JSONDecode = function(_, value) return serde.decode("json", value) end }
services.EncodingService = { Base64Decode = function(_, value) return buffer.fromstring(base64.decode(buffer.tostring(value))) end }
local player = { Name = "Player1", IsA = function(_, class) return class == "Player" end }
services.Players = { GetPlayers = function() return { player } end }
player.Parent = services.Players
services.SerializationService = { DeserializeInstancesAsync = function()
	if yieldDeserialize then coroutine.yield() end
	return { { Destroy = function() destroyed += 1 end } }
end }
local module = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgePlayTeleport.module.luau"), { environment = {
	game = { GetService = function(_, name) return assert(services[name], name) end },
	os = { clock = function() return now end }, task = { spawn = function() end, wait = coroutine.yield },
	Instance = { new = function(class)
		assert(class == "BindableFunction")
		nativeRelay = { SetAttribute = function(self, key, value) self[key] = value end, Destroy = function(self) self.destroyed = true end }
		return nativeRelay
	end },
	typeof = function(value) return if value == player then "Instance" else typeof(value) end,
} })()
local guiEvents = {}
local content = {
	installShim = function(_, callback) request = callback; return { Destroy = function() end } end,
	prepare = function() preparedCount += 1; return {} end,
	stageClients = function() return { Parent = "staged", Destroy = function() end } end,
	commit = function(_, _, _, guard) guard(); committed += 1; return 3 end,
	finishTemplates = function() end,
	pauseClient = function() return {} end,
	prepareClient = function() return {} end,
	clearClient = function() end,
	resumeClient = function() resumed += 1 end,
	arriveClient = function(_, _, guard)
		if yieldArrival then coroutine.yield() end
		guard()
		arrivals += 1
	end,
}
local function runtime(role)
	return module.create({ role = role, runtimeId = role, state = State, content = content, codec = {}, shimTemplate = {},
		gui = { create = function() return {
			show = function() table.insert(guiEvents, "show") end,
			fail = function(_, stop) table.insert(guiEvents, if stop then "stop" else "cancel") end,
			dismiss = function() table.insert(guiEvents, "dismiss") end,
			arrive = function() table.insert(guiEvents, "arrive") end,
			get = function() return {} end,
			native = function() return nil end,
		} end } })
end
local function send(api, role, action, values)
	values = values or {}
	values.runtimeId, values.action = role, action
	return api.handle(values)
end
local server = runtime("play-server")
assert(request, "the explicit API is published at server startup, before arm or arrival")
local client = runtime("play-client")
local edit = runtime("edit")
assert(not pcall(send, edit, "edit", "arm", {}))
assert(not pcall(send, server, "different-runtime", "arm", {}))
isEdit = true
assert(not pcall(send, server, "play-server", "arm", {}))
isEdit = false
send(server, "play-server", "arm", {})
local relayPath = send(server, "play-server", "nativeBridge").path
assert(relayPath[1] == "ServerStorage" and nativeRelay.Parent == services.ServerStorage)
send(server, "play-server", "nativeBridge", { scope = "owned-launch" })
assert(nativeRelay.__ReniumNative == "owned-launch")
send(server, "play-server", "nativeBridge")
assert(nativeRelay.__ReniumNative == "play-server", "reconnection must bind the existing relay to its new native owner")
local nativeCall = coroutine.create(function() nativeRelay.OnInvoke("12", nil, player) end)
assert(coroutine.resume(nativeCall))
assert(coroutine.status(nativeCall) == "suspended")
local nativeId = send(server, "play-server", "status").pending.id
local accept = coroutine.create(function() send(server, "play-server", "acceptNative", { id = nativeId }) end)
assert(coroutine.resume(accept))
assert(coroutine.resume(nativeCall) and coroutine.status(nativeCall) == "dead")
assert(coroutine.resume(accept) and coroutine.status(accept) == "dead")
send(server, "play-server", "fail", { id = nativeId })
nativeCall = coroutine.create(function() nativeRelay.OnInvoke("12", nil, player) end)
assert(coroutine.resume(nativeCall))
nativeId = send(server, "play-server", "status").pending.id
send(server, "play-server", "fail", { id = nativeId, error = "Destination permission denied" })
local nativeOk, nativeError = coroutine.resume(nativeCall)
assert(not nativeOk and string.find(tostring(nativeError), "Destination permission denied", 1, true))
for _, args in { { 0, { player } }, { 12, {} }, { 12, { player, player } }, { 12, { {} } }, { 12, { player }, {} } } do
	assert(not pcall(request, table.unpack(args)), "invalid API input must not create a pending attempt")
	assert(send(server, "play-server", "status").pending == nil)
end
local function startAttempt()
	local receipt = request(12, { player }, nil)
	assert(receipt.status == "queued" and receipt.trigger == "interim-api")
	assert(not pcall(request, 13, { player }, nil), "a busy request cannot overwrite the first group")
	now += 1
	local status = send(server, "play-server", "status")
	return status.pending.id
end
local function stage(id)
	send(server, "play-server", "begin", { id = id, placeId = 12, totalBytes = 32, manifest = { groups = table.create(13, {}) } })
	assert(not pcall(send, server, "play-server", "prepare", { id = id }))
	assert(not pcall(send, server, "play-server", "chunk", { id = id, offset = 1, bytes = "AAAA" }))
	send(server, "play-server", "chunk", { id = id, offset = 0, bytes = base64.encode(string.rep("x", 32)) })
end
local id = startAttempt()
stage(id)
send(server, "play-server", "prepare", { id = id })
assert(preparedCount == 1 and committed == 0)
send(server, "play-server", "commitBoundary", { id = id })
assert(send(server, "play-server", "commit", { id = id }).replicatedCount == 3)
assert(not pcall(send, server, "play-server", "commit", { id = id }), "commit cannot replay")
send(server, "play-server", "complete", { id = id })
server.cleanup(false)
assert(send(server, "play-server", "arm", {}).emulatedPlaceId == 12,
	"a completed compatibility hop must survive disconnect without requiring Stop")
id = startAttempt()
stage(id)
send(server, "play-server", "fail", { id = id - 1 })
send(server, "play-server", "prepare", { id = id })
assert(preparedCount == 2, "late failure must not discard the current upload")
assert(send(server, "play-server", "fail", { id = id }).stopRequired == false)
id = startAttempt()
stage(id)
yieldDeserialize = true
local suspended = coroutine.create(function() send(server, "play-server", "prepare", { id = id }) end)
assert(coroutine.resume(suspended))
server.cleanup(false)
assert(not coroutine.resume(suspended), "late deserialize cannot apply after disconnect")
assert(committed == 1 and destroyed == 3)
assert(send(server, "play-server", "arm", {}).emulatedPlaceId == 12,
	"a replacement daemon must recover the committed compatibility destination")
assert(not pcall(send, client, "play-client", "arm", {}))
send(client, "play-client", "loading", { id = 1 })
assert(not pcall(send, client, "play-client", "arrive", { id = 1 }))
send(client, "play-client", "pause", { id = 1 })
send(client, "play-client", "fail", { id = 1, stopRequired = false })
assert(resumed == 1 and guiEvents[#guiEvents] == "cancel")
send(client, "play-client", "loading", { id = 2 })
send(client, "play-client", "pause", { id = 2 })
send(client, "play-client", "clear", { id = 2 })
send(client, "play-client", "arrive", { id = 2 })
assert(arrivals == 1 and resumed == 2 and guiEvents[#guiEvents] == "arrive")
send(client, "play-client", "loading", { id = 3 })
send(client, "play-client", "pause", { id = 3 })
send(client, "play-client", "fail", { id = 3, stopRequired = true })
assert(guiEvents[#guiEvents] == "stop" and resumed == 2)
assert(not pcall(send, client, "play-client", "loading", { id = 4 }))
local reconnecting = runtime("play-client")
send(reconnecting, "play-client", "loading", { id = 1 })
reconnecting.cleanup(false)
send(reconnecting, "play-client", "loading", { id = 2 })
send(reconnecting, "play-client", "pause", { id = 2 })
send(reconnecting, "play-client", "clear", { id = 2 })
yieldArrival = true
local arriving = coroutine.create(function() send(reconnecting, "play-client", "arrive", { id = 2 }) end)
assert(coroutine.resume(arriving))
reconnecting.cleanup(false)
assert(not coroutine.resume(arriving), "late client replication cannot resume after disconnect")
assert(arrivals == 1 and resumed == 2 and guiEvents[#guiEvents] == "stop")
assert(not pcall(send, reconnecting, "play-client", "loading", { id = 3 }))
server.cleanup(true)
assert(not pcall(send, server, "play-server", "status"))
assert(not pcall(request, 12, { player }, nil), "an unloaded API cannot enqueue travel")
local installShim = content.installShim
content.installShim = function() error("The game already owns ReniumTeleport") end
local collision = runtime("play-server")
content.installShim = installShim
local collisionStatus = send(collision, "play-server", "arm")
assert(not collisionStatus.compatibility and collisionStatus.compatibilityError)
send(collision, "play-server", "nativeBridge")
local collisionCall = coroutine.create(function() nativeRelay.OnInvoke("12", nil, player) end)
assert(coroutine.resume(collisionCall), "a compatibility name collision must preserve native teleport")
local collisionId = send(collision, "play-server", "status").pending.id
local staged, collisionError = pcall(send, collision, "play-server", "begin", {
	id = collisionId, placeId = 12, totalBytes = 32, manifest = { groups = table.create(13, {}) },
})
assert(not staged and string.find(tostring(collisionError), "game already owns", 1, true))
assert(send(collision, "play-server", "status").phase == "loading", "refuse compatibility before staging or clearing content")
send(collision, "play-server", "fail", { id = collisionId, error = "Fixture ended" })
assert(not coroutine.resume(collisionCall))
collision.cleanup(true)
print("teleport lifecycle workloads passed")
