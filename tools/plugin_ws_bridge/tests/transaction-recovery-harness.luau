-- Load the entire checked-out module and dependencies. Only expose locals in
-- memory; mock the engine boundary and snapshot capture, never the repository.
local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")
local function gate(h)
    local threads, warnings, Config = {}, {}, {}
    local function resume(thread)
        local ok, detail = coroutine.resume(thread)
        assert(ok, tostring(detail))
    end
    local function spawn(callback)
        local thread = coroutine.create(callback)
        table.insert(threads, thread)
        resume(thread)
        return thread
    end
    local function event()
        local waiters = {}
        return {
            Event = { Wait = function()
                table.insert(waiters, coroutine.running())
                coroutine.yield('event')
            end },
            Fire = function()
                local pending = waiters
                waiters = {}
                for _, thread in pending do resume(thread) end
            end,
            Destroy = function() end,
        }
    end
    local source = fs.readFile('tools/plugin_ws_bridge/BridgeConnection.module.luau'):gsub('\r\n','\n')
    local first = assert(source:find('\tlocal drainExclusiveRequestQueue\n',1,true))
    local last = assert(source:find('\n\tlocal function validatedRequest',first,true))
    local prefix = 'local exclusiveRequestBusy = false\nlocal exclusiveRequestQueue = {}\nlocal exclusiveIdleCallbacks = {}\nlocal activeExclusiveRequest\n'
    local suffix = [[
return {
 busy = function() return exclusiveRequestBusy end,
 request = function(callback)
  table.insert(exclusiveRequestQueue, {params=callback})
  drainExclusiveRequestQueue()
 end,
}
]]
    local g = luau.load(prefix .. source:sub(first,last-1) .. suffix, {environment={
        Config=Config, task={spawn=spawn}, Instance={new=function() return event() end},
        warn=function(message) table.insert(warnings,message) end,
        executeRequest=function(_,_,_,_,callback)
            h.setGeneration(h.ownerGeneration or 7)
            local ok, detail=pcall(callback)
            h.setGeneration(nil)
            if not ok then table.insert(warnings,tostring(detail)) end
        end,
    }, debugName='production-exclusive-queue'})()
    g.threads, g.warnings, g.spawn, g.resume = threads, warnings, spawn, resume
    h.ctx.afterBridgeExclusiveIdle=Config.afterBridgeExclusiveIdle
    do
        local lock=fs.readFile('tools/plugin_ws_bridge/BridgeSessionLock.module.luau')
        local a=assert(lock:find('\tfunction api.capture(',1,true))
        local b=assert(lock:find('\n\tfunction api.isLockInstance',a,true))
        local api={owns=function() return h.owns ~= false end}
        local setOwnership = luau.load("local ownershipGeneration = 7\n" .. lock:sub(a,b-1)
            .. "\nreturn function(generation) ownershipGeneration = generation end", {environment={api=api}})()
        h.changeOwnershipGeneration = function(generation)
            setOwnership(generation)
            h.ownerGeneration = generation
        end
        local runtime = fs.readFile("tools/plugin_ws_bridge/BridgePluginRuntime.module.luau"):gsub("\r\n", "\n")
        local first = assert(runtime:find("assertSessionOwnership = function(generation)", 1, true))
        local last = assert(runtime:find("expectParentChange =", first, true))
        local bindings = luau.load("local activeExclusiveSessionGeneration\nreturn { setGeneration = function(g) activeExclusiveSessionGeneration = g end, getGeneration = function() return activeExclusiveSessionGeneration end," .. runtime:sub(first, last - 1) .. "}", { environment = { sessionLock = api } })()
        h.setGeneration, h.getGeneration = bindings.setGeneration, bindings.getGeneration
        h.ctx.assertSessionOwnership = bindings.assertSessionOwnership
        h.ctx.captureSessionOwnership = bindings.captureSessionOwnership
        h.ctx.withSessionOwnership = bindings.withSessionOwnership
        h.ctx.currentSessionOwnership = bindings.currentSessionOwnership

    end
    function g.call(callback)
        local result
        local completed = event()
        g.request(function()
            result = table.pack(pcall(callback))
            completed:Fire()
        end)
        if result == nil then completed.Event:Wait() end
        if not result[1] then error(result[2], 0) end
        return table.unpack(result, 2, result.n)
    end
    return g
end

local function harness(mutate, trackJournalOwnership, enumOverride)
	local h = { now = 0, delayed = {}, history = {}, expectations = {}, invalidations = {}, finishes = 0 }
	h.scriptEditor = { FindScriptDocument = function() return nil end, GetEditorSource = function(_, instance) return instance.Source end }
	local realGame = roblox.Instance.new("DataModel")
	local historyService = {
		TryBeginRecording = function(_, name) return name end,
		FinishRecording = function(_, recording, operation)
			table.insert(h.history, { recording = recording, operation = operation })
			if h.failHistory then error("engine FinishRecording rejected", 0) end
		end,
	}
	local fakeGame = { GetService = function(_, name)
		if name == "ScriptEditorService" then return h.scriptEditor end
		if name == "RunService" then return { Heartbeat = { Wait = function() end } } end
		if name == "CollectionService" then return {
			GetTags = function(_, instance) return table.clone(instance.tags or {}) end,
			HasTag = function(_, instance, tag) return table.find(instance.tags or {}, tag) ~= nil end,
			AddTag = function(_, instance, tag)
				if h.tagWrite then h.tagWrite() end
				instance.tags = instance.tags or {}; table.insert(instance.tags, tag)
			end,
			RemoveTag = function(_, instance, tag)
				if h.tagWrite then h.tagWrite() end
				table.remove(instance.tags, table.find(instance.tags, tag))
			end,
		} end
		if name == "ChangeHistoryService" then return historyService end
		if name == "EncodingService" or name == "SerializationService" then return {} end
		if name == "Selection" then return { Get = function() return {} end } end
		return realGame:GetService(name)
	end }
	local env = {
		game = fakeGame, Enum = enumOverride or roblox.Enum, Instance = roblox.Instance,
		Vector3 = roblox.Vector3, CFrame = roblox.CFrame, Color3 = roblox.Color3,
		script = { Parent = setmetatable({}, { __index = function(_, name) return name end }) },
		os = { clock = function() return h.now end, time = function() return h.now end },
		task = {
			delay = function(seconds, callback) table.insert(h.delayed, { at = h.now + seconds, run = callback }) end,
			defer = function(callback) table.insert(h.delayed, { at = h.now, run = callback }) end,
		},
	}
	local modules = { RbxDom = {} }
	local function loadModule(name)
		if modules[name] then return modules[name] end
		local source = fs.readFile("tools/plugin_ws_bridge/" .. name .. ".module.luau"):gsub("\r\n", "\n")
		if name == "BridgeEditorSync" then
			if mutate then source = mutate(source) end
			source = source:gsub("\n\treturn api\nend", "\n\tapi.reviewRecordOutcome = transactionOutcomes.record\n\treturn api\nend")
			source = source:gsub("return BridgeEditorSync%s*$", [[
BridgeEditorSync.review = {
 sessions = editorTransactions, imports = binaryImports, failures = cleanupFailures, recoveries = transactionRecoveries,
 state = TransactionState, expire = expireSession, arm = armSessionExpiry,
 prune = pruneExpiredSessions, beginOp = beginSessionOperation, endOp = endSessionOperation,
 overlay = ReferenceOverlay, diagnostics = recoveryDiagnostics,
 setResolver = function(resolver) resolvePathSegments = resolver end,
}
return BridgeEditorSync
]])
		end
		modules[name] = luau.load(source, { environment = env, debugName = name })()
		return modules[name]
	end
	env.require = loadModule
	local module = loadModule("BridgeEditorSync")
	h.r = module.review
	-- Exercise real tracker journal ownership too: a retired transaction must
	-- release the active journal slot before the next begin can succeed.
	local trackerSource = fs.readFile("tools/plugin_ws_bridge/BridgeStudioChanges.module.luau")
	local journalFirst = assert(trackerSource:find("\tfunction api.beginChangeJournal(", 1, true))
	local journalLast = assert(trackerSource:find("\tlocal function clearPropertyChangesForService(", journalFirst, true))
	local journalApi = { finishNativeImportObservations = function() end }
	h.journals = { state = { persistentTracking = true }, retiring = {} }
	luau.load(trackerSource:sub(journalFirst, journalLast - 1), { environment = {
		api = journalApi, state = h.journals.state, retiringJournals = h.journals.retiring,
		releaseJournalObservers = function() end,
	} })()
	h.r.state.captureSnapshot = function() return { groups = {}, originalByPath = {}, originalRoots = {}, properties = {}, metadata = {}, scriptDocuments = {} } end
	h.attributeObservation = { active = true, generation = 0 }
	h.ctx = {
		stats = {}, allowedServices = { Workspace = true }, maxChangesPerRequest = 5000,
		runtimeId = "recovery-test-plugin-session",
		assertSessionOwnership = function() end,
		studioChangeGeneration = function() return 0 end,
		nativeExportGeneration = function() return h.exportGeneration or 0 end,
		attributeObservation = function() return h.attributeObservation end,
		beginStudioChangeJournal = if trackJournalOwnership then journalApi.beginChangeJournal else function() end,
		drainStudioChangeJournal = if trackJournalOwnership then journalApi.drainChangeJournal else function() return {} end,
		retireStudioChangeJournal = journalApi.retireChangeJournal,
		expireStudioChangeJournal = function(id) h.expiredJournal = id; journalApi.expireChangeJournal(id) end,
		finishStudioChangeJournal = function(id)
			h.finishes += 1
			if h.failJournal then error("journal finish rejected", 0) end
			return if trackJournalOwnership then journalApi.finishChangeJournal(id) else {}
		end,
		finishEditorTransactionExpectation = function(id)
			if h.failExpectation then error("expectation finish rejected", 0) end
			table.insert(h.expectations, id)
		end,

		invalidateService = function(name)
			if h.failInvalidate then error("invalidate rejected", 0) end
			table.insert(h.invalidations, name)
		end,
		beginStudioChangeSuppression = function() end, endStudioChangeSuppression = function() end,
		expectParentChange = function() end,
		finishNativeImportObservations = function() return {} end,
		beginNativeImportObservations = function() end,
	}
	h.gate = gate(h)
	h.api = module.create(h.ctx)
	local withLease = h.api.withRequestLease
	h.api.withRequestLease = function(...)
		local args = table.pack(...)
		return h.gate.call(function() return withLease(table.unpack(args, 1, args.n)) end)
	end
	for _, name in { "finishRequestLeaseCancellation", "cleanup" } do
		local operation = h.api[name]
		h.api[name] = function(...)
			local args = table.pack(...)
			return h.gate.call(function() return operation(table.unpack(args, 1, args.n)) end)
		end
	end
	h.game, h.realGame, h.env, h.modules = fakeGame, realGame, env, modules
	function h.begin(id)
		h.api.withRequestLease("owner", false, h.api.beginTransaction, { transactionId = id, services = { "Workspace" } })
		return h.r.sessions[id]
	end
	function h.advance(untilTime)
		while true do
			table.sort(h.delayed, function(a,b) return a.at < b.at end)
			if not h.delayed[1] or h.delayed[1].at > untilTime then break end
			local job = table.remove(h.delayed, 1)
			h.now = job.at
			job.run()
		end
		h.now = untilTime
	end
	return h
end

local function outsideNode(h, name, parent)
	local values = { Name = name, Parent = parent, attributes = {}, children = {} }
	local node = setmetatable({}, {
		__index = values,
		__newindex = function(instance, key, value)
			if key == "Parent" and values.failParent then error("injected parent rejection", 0) end
			values[key] = value
			if key == "Parent" and type(value) == "table" and value.children and not table.find(value.children, instance) then
				table.insert(value.children, instance)
			end
			if key == "Parent" and h.parentWrite then h.parentWrite(instance, value) end
		end,
	})
	function node:GetChildren()
		local children = {}
		for _, child in ipairs(self.children) do if child.Parent == self then table.insert(children, child) end end
		return children
	end
	function node:GetDescendants() return self:GetChildren() end
	function node:IsA() return false end
	function node:IsDescendantOf(ancestor)
		if ancestor == h.game then ancestor = h.realGame end
		local current = self.Parent
		while current do
			if current == ancestor then return true end
			current = current.Parent
		end
		return false
	end
	function node:GetAttributes() return table.clone(self.attributes) end
	function node:GetAttribute(name) return self.attributes[name] end
	function node:SetAttribute(name, value)
		if h.attributeWrite then h.attributeWrite() end
		self.attributes[name] = value
	end
	function node:GetFullName() return self.Name end
	function node:Destroy()
		if self.failDestroy then error("injected Destroy rejection", 0) end
		self.Parent = nil
		self.destroyed = true
	end
	return node
end


return { harness = harness, outsideNode = outsideNode }
