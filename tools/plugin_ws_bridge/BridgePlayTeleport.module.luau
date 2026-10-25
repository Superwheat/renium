local Teleport = {}
local MAX_BYTES = 128 * 1024 * 1024

function Teleport.create(context)
	local RunService = game:GetService("RunService")
	local HttpService = game:GetService("HttpService")
	local EncodingService = game:GetService("EncodingService")
	local state = context.state.create(os.clock)
	local gui = if context.role == "play-client" then context.gui.create(context.runtimeId) else nil
	local upload, roots, prepared, bundle, paused, shim, shimError, clientSettings
	local clientId, clientPhase, clientTouched = 0, "idle", os.clock()
	local active, generation = true, 0
	local nativeRelay
	local routeRelay
	local routeJoinRelay
	local emulatedPlaceId
	local nativeRequests = {}
	local requestQueue, queuedPlayers = {}, {}
	local function finishNative(id, message)
		local pending = nativeRequests[id]
		if pending then
			pending.done, pending.error = true, message
		end
		return pending
	end
	local function discard()
		if roots then
			for _, root in roots do
				root:Destroy()
			end
		end
		if bundle and bundle.Parent == nil then
			bundle:Destroy()
		end
		bundle = nil
		roots, prepared, upload = nil, nil, nil
	end
	local function captureOptions(options)
		local data = if options then options:GetTeleportData() else nil
		local encoded = HttpService:JSONEncode({ teleportData = data })
		assert(#encoded <= 64 * 1024, "Teleport data exceeds 64 KiB")
		local reserved = options ~= nil and (options.ShouldReserveServer
			or options.ReservedServerAccessCode ~= "" or options.ServerInstanceId ~= "")
		return HttpService:JSONDecode(encoded), reserved
	end
	local armedOnce = false
	local unavailableReason
	local function request(place, players, options)
		local deadline = os.clock() + 30
		while active and not armedOnce and state.phase == "off" and not RunService:IsEdit() do
			assert(os.clock() < deadline, "Renium teleport is not armed; start Play with teleport enabled")
			task.wait()
		end
		assert(active and not RunService:IsEdit() and state.phase ~= "off" and state.phase ~= "failed",
			unavailableReason or "Renium teleport is unavailable")
		assert(type(place) == "number" and place > 0 and place % 1 == 0 and place <= 9007199254740991, "Invalid destination ID")
		assert(type(players) == "table" and #players > 0 and #players <= 8, "Teleport requires a player array")
		assert(options == nil or typeof(options) == "Instance" and options:IsA("TeleportOptions"), "Invalid TeleportOptions")
		local names, seen = {}, {}
		for key, player in players do
			assert(type(key) == "number" and key % 1 == 0 and key >= 1 and key <= #players, "Teleport requires a player array")
			assert(typeof(player) == "Instance" and player:IsA("Player") and player.Parent == game:GetService("Players"), "Player is not connected to this server")
			assert(not seen[player], "Duplicate teleport player")
			assert(not queuedPlayers[player] and not (state.pending and state.pending.data[player.Name]),
				"This player already has a pending teleport")
			seen[player] = true
			table.insert(names, player.Name)
		end
		assert(#names == #players, "Teleport requires a dense player array")
		local data, reserved = captureOptions(options)
		-- Keep independent callers in order while the current group travels.
		-- A second player's click must not overwrite or fail the first attempt.
		local entry, started = {}, generation
		table.insert(requestQueue, entry)
		for player in seen do
			queuedPlayers[player] = entry
		end
		deadline = os.clock() + 180
		while active and generation == started and not RunService:IsEdit()
			and state.phase ~= "off" and state.phase ~= "failed" and os.clock() < deadline
			and (requestQueue[1] ~= entry or state.phase ~= "idle") do
			task.wait()
		end
		table.remove(requestQueue, table.find(requestQueue, entry))
		for player in seen do
			queuedPlayers[player] = nil
		end
		assert(active and generation == started and not RunService:IsEdit() and state.phase == "idle",
			unavailableReason or "The queued teleport session ended")
		assert(os.clock() < deadline, "Queued teleport timed out")
		for player in seen do
			assert(player.Parent == game:GetService("Players"), "Player left while waiting to teleport")
		end
		for _, name in names do
			state.observe(place, name, data, reserved)
		end
		state.phase = "loading"
		return { id = state.id, status = "queued", trigger = "interim-api" }
	end
	local function nativeRequest(place, options, players)
		local queued = request(tonumber(place), players, options)
		state.pending.native = true
		local snapshot = state.pending
		local data = snapshot.data[snapshot.players[1]].teleportData
		local members = {}
		for _, player in players do
			table.insert(members, player.UserId)
		end
		local join = { SourcePlaceId = game.PlaceId, SourceGameId = game.GameId,
			TeleportData = data, Members = members }
		local pending = { done = false, completed = false }
		nativeRequests[queued.id] = pending
		local deadline = os.clock() + 180
		while active and not pending.done and os.clock() < deadline do
			task.wait()
		end
		nativeRequests[queued.id] = nil
		pending.completed = true
		assert(active and pending.done, "The owned Studio teleport session ended")
		assert(not pending.error, pending.error)
		return join, data, tostring(queued.id)
	end
	if context.role == "play-server" then
		-- Compatibility is optional: a game's module with this name must not
		-- prevent the bridge or native TeleportService from starting.
		local ok, installed = pcall(context.content.installShim, context.shimTemplate, request)
		if ok then
			shim = installed
		else
			shimError = tostring(installed)
		end
	end
	local function clientFailure(params)
		if params.id < clientId then
			return { ok = true }
		end
		clientId = params.id
		local stopRequired = params.stopRequired == true or clientPhase == "arriving" or clientPhase == "cleared"
		if not stopRequired and paused then
			context.content.resumeClient(paused)
			paused = nil
		end
		clientPhase = if stopRequired then "failed" else "idle"
		gui.fail(params.error, stopRequired)
		return { ok = true, stopRequired = stopRequired, nativeGui = gui.native() }
	end
	local function handleClient(params)
		assert(type(params.id) == "number" and params.id % 1 == 0 and params.id >= clientId
			and params.id <= 9007199254740991, "Stale client teleport")
		clientTouched = os.clock()
		if params.action == "nativeGui" then
			assert(params.id == clientId, "Stale teleport GUI notification")
			gui.invokeNative()
			return { ok = true }
		end
		if params.action == "fail" then
			return clientFailure(params)
		end
		assert(clientPhase ~= "failed", "Stop Play after an incomplete teleport")
		if params.action == "loading" then
			assert(clientPhase == "idle" or clientPhase == "loading" and params.id == clientId, "Client teleport is busy")
			clientId, clientPhase = params.id, "loading"
			gui.show(params)
		elseif params.action == "pause" then
			assert(params.id == clientId and clientPhase == "loading", "Client teleport is not loading")
			clientSettings = context.content.prepareClient(params.clientGroups, context.codec, context.collisionGroups)
			paused = context.content.pauseClient(gui.get())
			clientPhase = "paused"
		elseif params.action == "clear" then
			assert(params.id == clientId and clientPhase == "paused", "Client teleport is not paused")
			clientPhase = "cleared"
			context.content.clearClient(gui.get(), clientSettings)
		elseif params.action == "arrive" then
			assert(params.id == clientId and clientPhase == "cleared", "Client teleport has not cleared source content")
			clientPhase = "arriving"
			local started = generation
			local function guard()
				assert(active and generation == started and clientPhase == "arriving" and not RunService:IsEdit(), "Play changed during teleport")
			end
			context.content.arriveClient(params, gui.get(), guard, clientSettings)
			guard()
			context.content.resumeClient(paused)
			paused = nil
			clientSettings = nil
			clientPhase = "idle"
			gui.arrive()
		else
			error("Unsupported client teleport operation")
		end
		return { ok = true, nativeGui = gui.native() }
	end
	local function begin(params)
		state.expect(params.id, "loading")
		assert(shim, shimError or "Teleport compatibility module is unavailable")
		assert(params.placeId == state.pending.placeId, "Destination changed")
		assert(type(params.totalBytes) == "number" and params.totalBytes >= 32
			and params.totalBytes <= MAX_BYTES and params.totalBytes % 1 == 0, "Invalid teleport payload size")
		assert(type(params.manifest) == "table" and type(params.manifest.groups) == "table"
			and #params.manifest.groups == 13, "Invalid teleport manifest")
		upload = { bytes = buffer.create(params.totalBytes), received = 0, params = params }
		state.phase = "uploading"
	end
	local function chunk(params)
		state.expect(params.id, "uploading")
		assert(params.offset == upload.received and type(params.bytes) == "string" and #params.bytes <= 1400000, "Out-of-order teleport chunk")
		local decoded = EncodingService:Base64Decode(buffer.fromstring(params.bytes))
		local size = buffer.len(decoded)
		assert(size > 0 and size <= 1024 * 1024 and upload.received + size <= buffer.len(upload.bytes), "Oversized teleport chunk")
		buffer.copy(upload.bytes, upload.received, decoded)
		upload.received += size
	end
	local function prepare(params)
		state.expect(params.id, "uploading")
		assert(upload.received == buffer.len(upload.bytes), "Teleport payload is incomplete")
		state.phase = "preparing"
		local incoming = game:GetService("SerializationService"):DeserializeInstancesAsync(upload.bytes)
		if not active or state.phase ~= "preparing" or state.id ~= params.id then
			for _, root in incoming do
				root:Destroy()
			end
			error("Teleport cancelled while deserializing")
		end
		roots = incoming
		prepared = context.content.prepare(roots, upload.params.manifest, context.codec, context.collisionGroups)
		bundle = context.content.stageClients(prepared, upload.params, context.shimTemplate)
		upload.bytes = nil
		state.phase = "prepared"
	end
	local function commit(params)
		state.expect(params.id, "clearing")
		local players = game:GetService("Players"):GetPlayers()
		assert(#players == #state.pending.players, "Players changed before teleport commit")
		for _, player in players do
			assert(state.pending.data[player.Name], "Players changed before teleport commit")
		end
		state.advance(params.id, "clearing", "committing")
		local function guard()
			assert(active and not RunService:IsEdit() and state.id == params.id and state.phase == "committing", "Teleport session ended during commit")
		end
		local count = context.content.commit(prepared, upload.params, bundle, guard, shim, nativeRelay)
		guard()
		state.phase = "arriving"
		emulatedPlaceId = upload.params.placeId
		discard()
		return { ok = true, replicatedCount = count }
	end
	local function failure(params)
		if params.id ~= state.id then
			return { ok = true, stopRequired = state.phase == "failed" }
		end
		local stopRequired = state.fail(params.id)
		finishNative(params.id, params.error or "Teleport failed")
		discard()
		return { ok = true, stopRequired = stopRequired }
	end
	local handlers = {
		nativeBridge = function(params)
			assert(state.phase == "idle", "Native teleport needs the armed server")
			if not nativeRelay then
				nativeRelay = Instance.new("BindableFunction")
				nativeRelay.Name = "__ReniumTeleportNative_" .. context.runtimeId
				nativeRelay:SetAttribute("__ReniumNative", params.scope or context.runtimeId)
				nativeRelay.OnInvoke = function(place, options, ...)
					if place == nil then
						return
					end
					-- A thrown OnInvoke error is logged by Studio without resuming
					-- this native caller. Return it so the hook completes TeleportAsync.
					local ok, join, data, id = pcall(nativeRequest, place, options, { ... })
					if not ok then
						-- Let TeleportAsync finish yielding before its error continuation
						-- runs. Immediate failures otherwise throw into BindableFunction's
						-- nested invocation instead of returning to the teleport caller.
						task.wait()
						return tostring(join)
					end
					return join, data, id
				end
				nativeRelay.Parent = game:GetService("ServerStorage")
			end
			nativeRelay:SetAttribute("__ReniumNative", params.scope or context.runtimeId)
			nativeRelay:SetAttribute("__ReniumNativePump", params.scope)
			if params.scope then
				-- Wake launch-time native callers on this model's own scheduler.
				-- The relay is ready before this deferred invocation can run.
				local relay = nativeRelay
				task.defer(function()
					if relay.Parent then
						relay:Invoke()
					end
				end)
			end
			return { ok = true, path = { "ServerStorage", nativeRelay.Name }, modelName = game.Name }
		end,
		acceptNative = function(params)
			local pending = finishNative(params.id, nil)
			local deadline = os.clock() + 5
			while pending and not pending.completed do
				assert(os.clock() < deadline, "Native teleport caller did not complete")
				task.wait()
			end
		end,
		completeRoute = function(params)
			state.advance(params.id, "loading", "arriving")
			state.complete(params.id)
		end,
		addRouteClient = function()
			game:GetService("StudioTestService"):AddPlayers(1)
		end,
		arm = function()
			state.arm()
			armedOnce = true
			unavailableReason = nil
			return { ok = true, compatibility = shim ~= nil, compatibilityError = shimError,
				emulatedPlaceId = emulatedPlaceId }
		end,
		status = function()
			return { ok = true, phase = state.phase, pending = state.poll() }
		end,
		begin = begin, chunk = chunk, prepare = prepare, commit = commit, fail = failure,
		commitBoundary = function(params)
			state.advance(params.id, "prepared", "clearing")
			state.committedId = params.id
		end,
		complete = function(params)
			state.expect(params.id, "arriving")
			context.content.finishTemplates()
			state.complete(params.id)
		end,
		disarm = function(params)
			armedOnce = true
			unavailableReason = params.reason
			local stopRequired = state.phase ~= "idle" and state.fail(state.id)
			if not stopRequired then
				state.phase = "off"
			end
			discard()
		end,
	}
	local function handle(params)
		assert(params.runtimeId == context.runtimeId and active, "Teleport runtime changed")
		assert(RunService:IsStudio() and not RunService:IsEdit(), "Teleport is only available in Play")
		if params.action == "routeJoin" then
			if context.role == "play-server" then
				local identity = params.identity
				assert(type(identity) == "table" and type(identity.userId) == "number" and identity.userId < 0
					and identity.userId % 1 == 0 and type(identity.name) == "string" and type(identity.displayName) == "string",
					"Invalid test-player identity")
				local players = game:GetService("Players")
				assert(not players:GetPlayerByUserId(identity.userId), "This test player is already in the destination")
			end
			if routeJoinRelay then
				routeJoinRelay:Destroy()
			end
			routeJoinRelay = Instance.new("BindableFunction")
			routeJoinRelay.Name = "__ReniumRouteJoin_" .. context.runtimeId
			routeJoinRelay.OnInvoke = function()
				return params.join, params.join.TeleportData
			end
			routeJoinRelay.Parent = game:GetService("CoreGui")
			return { ok = true, path = { "CoreGui", routeJoinRelay.Name }, modelName = game.Name }
		end
		if params.action == "routeIdentity" then
			assert(context.role == "play-server", "Only the source server supplies player identity")
			local player = assert(game:GetService("Players"):FindFirstChild(params.playerName), "Teleport player left")
			return { ok = true, identity = { userId = player.UserId, name = player.Name, displayName = player.DisplayName } }
		end
		if params.action == "routeVerify" then
			local player = if context.role == "play-client" then game:GetService("Players").LocalPlayer
				else game:GetService("Players"):GetPlayerByUserId(params.userId)
			assert(player, "Routed player did not arrive")
			return { ok = true, join = player:GetJoinData(),
				identity = { userId = player.UserId, name = player.Name, displayName = player.DisplayName },
				data = if context.role == "play-client" then game:GetService("TeleportService"):GetLocalPlayerTeleportData() else nil }
		end
		if params.action == "routeArrival" then
			assert(context.role == "play-client", "Only a routed client can arrive")
			game:SetAttribute("__ReniumLaunchNonce", params.nonce)
			game:SetAttribute("__ReniumEditRuntimeId", params.owner)
			return { ok = true }
		end
		if params.action == "clientRoute" then
			assert(context.role == "play-client" and clientPhase == "loading" and params.id == clientId,
				"Client is not preparing this teleport")
			assert(type(params.configuration) == "table" and params.configuration.LaunchTask == "StartClient",
				"Invalid client route configuration")
			if routeRelay then
				routeRelay:Destroy()
			end
			routeRelay = Instance.new("BindableFunction")
			routeRelay.Name = "__ReniumTeleportRoute_" .. context.runtimeId
			routeRelay.OnInvoke = function()
				return params.configuration
			end
			routeRelay.Parent = game:GetService("CoreGui")
			return { ok = true, path = { "CoreGui", routeRelay.Name }, modelName = game.Name }
		end
		if params.action == "invokeRoute" then
			assert(params.kind == "client" or params.kind == "join", "Invalid route invocation")
			local relay = if params.kind == "client" then routeRelay else routeJoinRelay
			assert(relay and relay.Parent == game:GetService("CoreGui"), "Route relay ended")
			relay:GetAttribute("__ReniumRouteInvoke")
			return { ok = true }
		end
		if params.action == "nativeArrival" then
			local player = if context.role == "play-client" then game:GetService("Players").LocalPlayer
				else game:GetService("Players"):GetPlayers()[1]
			assert(player, "Native teleport player is not ready")
			if shim and params.mode == "native" then
				assert(params.placeId == game.PlaceId and params.gameId == game.GameId, "Native arrival identity changed")
				shim.Context.Value = HttpService:JSONEncode({ mode = "native", id = params.id,
					placeId = game.PlaceId, gameId = game.GameId, jobId = game.JobId,
					sourcePlaceId = params.sourcePlaceId, originPlaceId = params.originPlaceId,
					version = params.version, contentSource = params.contentSource, data = {} })
			end
			return { ok = true, join = player:GetJoinData(), data = if context.role == "play-client"
				then game:GetService("TeleportService"):GetLocalPlayerTeleportData() else nil }
		end
		if context.role == "play-client" then
			return handleClient(params)
		end
		assert(context.role == "play-server", "Teleport requires a play server or client")
		local handler = assert(handlers[params.action], "Unknown teleport operation")
		return handler(params) or { ok = true }
	end
	local function cleanup(unloading)
		generation += 1
		active = not unloading
		for id in nativeRequests do
			finishNative(id, "Renium disconnected during teleport")
		end
		if nativeRelay then
			nativeRelay:Destroy()
			nativeRelay = nil
		end
		if routeRelay then
			routeRelay:Destroy()
			routeRelay = nil
		end
		if routeJoinRelay then
			routeJoinRelay:Destroy()
			routeJoinRelay = nil
		end
		if unloading and shim then
			shim:Destroy()
		end
		local stopRequired = state.phase ~= "idle" and state.fail(state.id)
		if not stopRequired then
			state.phase = "off"
		end
		discard()
		if gui then
			if clientPhase == "loading" or clientPhase == "idle" then
				clientPhase = "idle"
				gui.dismiss()
			else
				clientPhase = "failed"
				gui.fail("Renium disconnected", true)
			end
		end
	end
	if context.role ~= "edit" then
		task.spawn(function()
			while active do
				task.wait(1)
				if gui and clientPhase ~= "idle" and clientPhase ~= "failed" and os.clock() - clientTouched > 180 then
					clientFailure({ id = clientId, error = "Teleport timed out", stopRequired = clientPhase ~= "loading" })
				elseif not gui and state.phase ~= "off" and state.phase ~= "idle" and state.phase ~= "failed" and os.clock() - state.touchedAt > 180 then
					failure({ id = state.id })
				end
			end
		end)
	end
	return { handle = handle, cleanup = cleanup }
end

return Teleport
