local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")

local B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
local B64_DEC = {}
for i = 1, #B64 do
	B64_DEC[string.byte(B64, i)] = i - 1
end

local function base64Encode(buf: buffer): string
	local len = buffer.len(buf)
	local out = table.create(math.ceil(len / 3) * 4)
	local n = 0
	for i = 0, len - 1, 3 do
		local b0 = buffer.readu8(buf, i)
		local b1 = if i + 1 < len then buffer.readu8(buf, i + 1) else 0
		local b2 = if i + 2 < len then buffer.readu8(buf, i + 2) else 0
		local triple = bit32.lshift(b0, 16) + bit32.lshift(b1, 8) + b2
		n += 1
		out[n] = string.sub(B64, bit32.rshift(triple, 18) + 1, bit32.rshift(triple, 18) + 1)
		n += 1
		out[n] = string.sub(B64, bit32.band(bit32.rshift(triple, 12), 63) + 1, bit32.band(bit32.rshift(triple, 12), 63) + 1)
		n += 1
		out[n] = if i + 1 < len
			then string.sub(B64, bit32.band(bit32.rshift(triple, 6), 63) + 1, bit32.band(bit32.rshift(triple, 6), 63) + 1)
			else "="
		n += 1
		out[n] = if i + 2 < len
			then string.sub(B64, bit32.band(triple, 63) + 1, bit32.band(triple, 63) + 1)
			else "="
	end
	return table.concat(out)
end

local function base64Decode(buf: buffer): buffer
	local text = buffer.tostring(buf)
	local len = #text
	if len == 0 then
		return buffer.create(0)
	end
	local pad = 0
	if string.sub(text, len, len) == "=" then pad += 1 end
	if len >= 2 and string.sub(text, len - 1, len - 1) == "=" then pad += 1 end
	local outLen = math.floor(len / 4) * 3 - pad
	local out = buffer.create(outLen)
	local w = 0
	for i = 1, len, 4 do
		local c0 = B64_DEC[string.byte(text, i)] or 0
		local c1 = B64_DEC[string.byte(text, i + 1)] or 0
		local c2 = B64_DEC[string.byte(text, i + 2)] or 0
		local c3 = B64_DEC[string.byte(text, i + 3)] or 0
		local triple = bit32.lshift(c0, 18) + bit32.lshift(c1, 12) + bit32.lshift(c2, 6) + c3
		if w < outLen then
			buffer.writeu8(out, w, bit32.band(bit32.rshift(triple, 16), 255))
			w += 1
		end
		if w < outLen then
			buffer.writeu8(out, w, bit32.band(bit32.rshift(triple, 8), 255))
			w += 1
		end
		if w < outLen then
			buffer.writeu8(out, w, bit32.band(triple, 255))
			w += 1
		end
	end
	return out
end

local function packNodes(nodes: { { number } }): string
	local buf = buffer.create(#nodes * 16)
	for i, node in ipairs(nodes) do
		local off = (i - 1) * 16
		buffer.writeu32(buf, off, node[1])
		buffer.writeu32(buf, off + 4, node[2])
		buffer.writeu32(buf, off + 8, node[3])
		buffer.writeu32(buf, off + 12, node[4])
	end
	return base64Encode(buf)
end

local function packU32(values: { number }): string
	local buf = buffer.create(#values * 4)
	for i, v in ipairs(values) do
		buffer.writeu32(buf, (i - 1) * 4, v)
	end
	return base64Encode(buf)
end

local function packF32(values: { number }): string
	local buf = buffer.create(#values * 4)
	for i, v in ipairs(values) do
		buffer.writef32(buf, (i - 1) * 4, v)
	end
	return base64Encode(buf)
end

local function packBool(values: { boolean }): string
	local buf = buffer.create(#values)
	for i, v in ipairs(values) do
		buffer.writeu8(buf, i - 1, if v then 1 else 0)
	end
	return base64Encode(buf)
end

local function packVec3(values: { { number } }): string
	local buf = buffer.create(#values * 12)
	for i, v in ipairs(values) do
		local off = (i - 1) * 12
		buffer.writef32(buf, off, v[1])
		buffer.writef32(buf, off + 4, v[2])
		buffer.writef32(buf, off + 8, v[3])
	end
	return base64Encode(buf)
end

local function buildHarness()
	local realGame = roblox.Instance.new("DataModel")
	local workspace = realGame:GetService("Workspace")
	local history = {}
	local exportNotices = {}
	local activeBulkExpected = nil
	local fakeGame = {
		GetService = function(_, name)
			if name == "EncodingService" then
				return {
					Base64Encode = function(_, b) return buffer.fromstring(base64Encode(b)) end,
					Base64Decode = function(_, b) return base64Decode(b) end,
				}
			end
			if name == "HttpService" then
				return {
					GenerateGUID = function() return "00000000-0000-0000-0000-000000000000" end,
					JSONEncode = function(_, v) return tostring(v) end,
				}
			end
			if name == "ScriptEditorService" then
				return {
					FindScriptDocument = function() return nil end,
					GetScriptDocuments = function() return {} end,
				}
			end
			if name == "RunService" then
				return { Heartbeat = { Wait = function() end } }
			end
			if name == "CollectionService" then
				return { GetTags = function() return {} end }
			end
			if name == "ChangeHistoryService" then
				return {
					TryBeginRecording = function(_, recName) return recName end,
					FinishRecording = function(_, rec, op)
						table.insert(history, { rec = rec, op = op })
					end,
				}
			end
			if name == "EncodingService" then
				return {
					Base64Encode = function(_, b) return buffer.fromstring(base64Encode(b)) end,
					Base64Decode = function(_, b) return base64Decode(b) end,
				}
			end
			if name == "SerializationService" then
				return {}
			end
			if name == "Selection" then
				return { Get = function() return {} end, Set = function() end }
			end
			return realGame:GetService(name)
		end,
	}
	local env: { [string]: any } = {
		game = fakeGame,
		Enum = roblox.Enum,
		Instance = {
			new = function(className: string)
				if className == "BindableEvent" then
					return {
						Event = { Connect = function() return { Disconnect = function() end } end },
						Fire = function() end,
						Destroy = function() end,
					}
				end
				return roblox.Instance.new(className)
			end,
		},
		Vector3 = roblox.Vector3,
		CFrame = roblox.CFrame,
		Color3 = roblox.Color3,
		script = { Parent = setmetatable({}, { __index = function(_, name) return name end }) },
		os = os,
		task = {
			delay = function() end,
			defer = function(fn) fn() end,
		},
	}
	local modules: { [string]: any } = {
		RbxDom = {
			findCanonicalPropertyDescriptor = function() return nil end,
			getContentPropertyAliases = function() return {} end,
		},
	}
	local function loadModule(name: string)
		if modules[name] then return modules[name] end
		local source = fs.readFile("tools/plugin_ws_bridge/" .. name .. ".module.luau"):gsub("\r\n", "\n")
		modules[name] = luau.load(source, { environment = env, debugName = name })()
		return modules[name]
	end
	env.require = loadModule
	local EditorSync = loadModule("BridgeEditorSync")
	local stats = {
		requests = 0, lastMs = 0, lastAtUnix = 0, lastOk = true,
		propertyUpdated = 0, attributeUpdated = 0, noops = 0, errors = 0,
		sourceCreated = 0, sourceUpdated = 0, sourceDeleted = 0,
		instanceCreated = 0, instanceReplaced = 0, instanceDeleted = 0,
	}
	local ctx = {
		stats = stats,
		allowedServices = { Workspace = true },
		maxChangesPerRequest = 5000,
		runtimeId = "bulk-test-session",
		assertSessionOwnership = function() end,
		captureSessionOwnership = function() return 1 end,
		currentSessionOwnership = function() return 1 end,
		withSessionOwnership = function(_, fn) return fn() end,
		studioChangeGeneration = function() return 0 end,
		nativeExportGeneration = function() return 0 end,
		attributeObservation = function() return { active = true, generation = 0 } end,
		beginStudioChangeJournal = function() end,
		drainStudioChangeJournal = function() return {} end,
		retireStudioChangeJournal = function() end,
		expireStudioChangeJournal = function() end,
		finishStudioChangeJournal = function() return {} end,
		finishEditorTransactionExpectation = function() end,
		invalidateService = function() end,
		beginStudioChangeSuppression = function() end,
		endStudioChangeSuppression = function() end,
		expectParentChange = function() end,
		expectPropertyEvent = function() end,
		expectAttributeEvent = function() end,
		cancelExpectedEvent = function() end,
		samplePropertyChange = function() end,
		beginNativeImportObservations = function() end,
		finishNativeImportObservations = function() return {} end,
		updateStatus = function() end,
		setBulkExpectedProperties = function(expected) activeBulkExpected = expected end,
		noteBulkExportChange = function(serviceName, propName)
			table.insert(exportNotices, serviceName .. "." .. propName)
		end,
	}
	local api = EditorSync.create(ctx)
	return api, workspace, function() return activeBulkExpected end, exportNotices
end

do
	local api, workspace, getBulkExpected, exportNotices = buildHarness()
	local folder = roblox.Instance.new("Folder")
	folder.Name = "Props"
	folder.Parent = workspace

	local parts = {}
	for i = 1, 5 do
		local p = roblox.Instance.new("Part")
		p.Name = "Twin"
		p.Transparency = if i == 5 then 0.5 else 0
		p.CanCollide = true
		p.Color = roblox.Color3.new(1, 0, 0)
		p.Size = roblox.Vector3.new(4, 1, 2)
		p.Parent = folder
		parts[i] = p
	end

	local begin = api.withRequestLease("lease-1", false, api.beginTransaction, {
		transactionId = "tx-bulk-1",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	assert(begin.ok == true, "bulk transaction begin failed")

	local stringsB64 = base64Encode(buffer.fromstring("Props\0Folder\0Twin\0Part"))
	local nodes = {
		{ 0, 1, 0, 1 },
		{ 1, 1, 2, 3 },
		{ 1, 2, 2, 3 },
		{ 1, 3, 2, 3 },
		{ 1, 4, 2, 3 },
		{ 1, 5, 2, 3 },
	}
	local result = api.withRequestLease("lease-1", false, api.applyBulkProperties, {
		transactionId = "tx-bulk-1",
		service = "Workspace",
		tree = {
			strings = stringsB64,
			stringCount = 4,
			nodeOffset = 0,
			nodeCount = #nodes,
			totalNodes = #nodes,
			nodes = packNodes(nodes),
		},
		columns = {
			{
				propertyName = "Transparency",
				encoding = "f32_const",
				count = 5,
				rows = packU32({ 2, 3, 4, 5, 6 }),
				value = 0.5,
			},
			{
				propertyName = "CanCollide",
				encoding = "bool",
				count = 3,
				rows = packU32({ 2, 3, 4 }),
				values = packBool({ false, true, false }),
			},
			{
				propertyName = "Size",
				encoding = "vector3",
				count = 2,
				rows = packU32({ 2, 6 }),
				values = packVec3({ { 2, 3, 4 }, { 8, 9, 10 } }),
			},
			{
				propertyName = "Color",
				encoding = "color3",
				count = 1,
				rows = packU32({ 3 }),
				values = packVec3({ { 0, 0.5, 1 } }),
			},
		},
	})
	assert(result.ok == true, "bulk apply failed")
	assert(result.expectedFields == 11, "expectedFields mismatch: " .. tostring(result.expectedFields))
	assert(result.verified == 11, "verified mismatch: " .. tostring(result.verified))
	assert(result.noops == 2, "expected 2 noops (Twin[5].Transparency and Twin[2].CanCollide), got " .. tostring(result.noops))
	assert(result.propertyUpdated == 9, "expected 9 updates, got " .. tostring(result.propertyUpdated))
	assert(getBulkExpected() ~= nil, "bulk expectations stay armed until commit drain")
	assert(#exportNotices == 4, "export invalidated once per column")

	for i = 1, 5 do
		assert(math.abs(parts[i].Transparency - 0.5) < 1e-5, "Transparency mismatch on part " .. i)
	end
	assert(parts[1].CanCollide == false and parts[2].CanCollide == true and parts[3].CanCollide == false)
	assert(parts[1].Size == roblox.Vector3.new(2, 3, 4) and parts[5].Size == roblox.Vector3.new(8, 9, 10))

	local result2 = api.withRequestLease("lease-1", false, api.applyBulkProperties, {
		transactionId = "tx-bulk-1",
		service = "Workspace",
		columns = {
			{
				propertyName = "Reflectance",
				encoding = "f32",
				count = 2,
				rows = packU32({ 2, 3 }),
				values = packF32({ 0.25, 0.75 }),
			},
		},
	})
	assert(result2.ok == true and result2.verified == 2 and result2.propertyUpdated == 2)
	assert(math.abs(parts[1].Reflectance - 0.25) < 1e-5 and math.abs(parts[2].Reflectance - 0.75) < 1e-5)

	local commit = api.withRequestLease("lease-1", false, api.commitTransaction, {
		transactionId = "tx-bulk-1",
	})
	assert(commit.ok == true and commit.state == "committed", "bulk commit failed")
	assert(getBulkExpected() == nil, "bulk expectations cleared on commit")
end

do
	local api, workspace, getBulkExpected = buildHarness()
	local part = roblox.Instance.new("Part")
	part.Name = "Box"
	part.Transparency = 0.25
	part.CanCollide = true
	part.Parent = workspace

	api.withRequestLease("lease-2", false, api.beginTransaction, {
		transactionId = "tx-bulk-rb",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local stringsB64 = base64Encode(buffer.fromstring("Box\0Part"))
	api.withRequestLease("lease-2", false, api.applyBulkProperties, {
		transactionId = "tx-bulk-rb",
		service = "Workspace",
		tree = {
			strings = stringsB64,
			stringCount = 2,
			nodeOffset = 0,
			nodeCount = 1,
			totalNodes = 1,
			nodes = packNodes({ { 0, 1, 0, 1 } }),
		},
		columns = {
			{
				propertyName = "Transparency",
				encoding = "f32_const",
				count = 1,
				rows = packU32({ 1 }),
				value = 0.75,
			},
			{
				propertyName = "CanCollide",
				encoding = "bool_const",
				count = 1,
				rows = packU32({ 1 }),
				value = false,
			},
		},
	})
	assert(math.abs(part.Transparency - 0.75) < 1e-5 and part.CanCollide == false)

	local rb = api.withRequestLease("lease-2", false, api.rollbackTransaction, {
		transactionId = "tx-bulk-rb",
	})
	assert(rb.ok == true and rb.state == "rolledBack", "bulk rollback failed")
	assert(math.abs(part.Transparency - 0.25) < 1e-5, "Transparency not restored on rollback")
	assert(part.CanCollide == true, "CanCollide not restored on rollback")
	assert(getBulkExpected() == nil, "bulk expectations cleared on rollback")
end

do
	local api, workspace = buildHarness()
	local model = roblox.Instance.new("Model")
	model.Name = "PkgModel"
	model.Parent = workspace
	local link = roblox.Instance.new("PackageLink")
	link.Name = "PackageLink"
	link.Parent = model
	local part = roblox.Instance.new("Part")
	part.Name = "Inside"
	part.Parent = model

	api.withRequestLease("lease-3", false, api.beginTransaction, {
		transactionId = "tx-bulk-pkg",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local stringsB64 = base64Encode(buffer.fromstring("PkgModel\0Model\0Inside\0Part"))
	local ok, err = pcall(api.withRequestLease, "lease-3", false, api.applyBulkProperties, {
		transactionId = "tx-bulk-pkg",
		service = "Workspace",
		tree = {
			strings = stringsB64,
			stringCount = 4,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			nodes = packNodes({ { 0, 1, 0, 1 }, { 1, 1, 2, 3 } }),
		},
		columns = {
			{
				propertyName = "Transparency",
				encoding = "f32_const",
				count = 1,
				rows = packU32({ 2 }),
				value = 0.5,
			},
		},
	})
	assert(not ok and string.find(tostring(err), "PackageLink", 1, true) ~= nil, "PackageLink must be rejected")
	api.withRequestLease("lease-3", false, api.rollbackTransaction, { transactionId = "tx-bulk-pkg" })
end

do
	local itemChangedCallbacks = {}
	local itemChangedSignal = {
		Connect = function(_, cb)
			itemChangedCallbacks[cb] = true
			return { Disconnect = function() itemChangedCallbacks[cb] = nil end }
		end,
		Fire = function(_, inst, prop)
			for cb in pairs(table.clone(itemChangedCallbacks)) do
				cb(inst, prop)
			end
		end,
	}
	local exportEvents = {}
	local trackedSource = fs.readFile("tools/plugin_ws_bridge/BridgeStudioChanges.module.luau"):gsub("\r\n", "\n")
	local realGame = roblox.Instance.new("DataModel")
	local workspace = realGame:GetService("Workspace")
	local part = roblox.Instance.new("Part")
	part.Name = "BulkObserved"
	part.Transparency = 0
	part.Parent = workspace

	local fakeWorkspace = {
		ClassName = "Workspace",
		Name = "Workspace",
		DescendantAdded = { Connect = function() return { Disconnect = function() end } end },
		DescendantRemoving = { Connect = function() return { Disconnect = function() end } end },
		Changed = { Connect = function() return { Disconnect = function() end } end },
		GetChildren = function() return { part } end,
		GetDescendants = function() return { part } end,
		GetFullName = function() return "Workspace" end,
	}
	local fakeGame = {
		ItemChanged = itemChangedSignal,
		GetService = function(_, name)
			if name == "Workspace" then
				return fakeWorkspace
			end
			if name == "CollectionService" then
				return {
					GetTags = function() return {} end,
					GetInstanceAddedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
					GetInstanceRemovedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
				}
			end
			if name == "RunService" then
				return { Heartbeat = { Connect = function() return { Disconnect = function() end } end } }
			end
			if name == "EncodingService" then
				return {
					Base64Encode = function(_, b) return buffer.fromstring(base64Encode(b)) end,
					Base64Decode = function(_, b) return base64Decode(b) end,
				}
			end
			if name == "HttpService" then
				return {
					GenerateGUID = function() return "00000000-0000-0000-0000-000000000000" end,
					JSONEncode = function(_, v) return tostring(v) end,
				}
			end
			if name == "ScriptEditorService" then
				return {
					GetScriptDocuments = function() return {} end,
					FindScriptDocument = function() return nil end,
					TextDocumentDidChange = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidOpen = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidClose = { Connect = function() return { Disconnect = function() end } end },
				}
			end
			if name == "ChangeHistoryService" then
				return {
					TryBeginRecording = function() return nil end,
					FinishRecording = function() end,
				}
			end
			if name == "Selection" then
				return {
					Get = function() return {} end,
					Set = function() end,
				}
			end
			return realGame:GetService(name)
		end,
	}

	local modules: { [string]: any } = {
		RbxDom = {
			findCanonicalPropertyDescriptor = function() return nil end,
			getContentPropertyAliases = function() return {} end,
			getPropertyNames = function() return { "Transparency", "Reflectance" } end,
		},
	}
	local env: { [string]: any } = {
		game = fakeGame,
		Enum = roblox.Enum,
		Instance = {
			new = function(className: string)
				if className == "BindableEvent" then
					return {
						Event = { Connect = function() return { Disconnect = function() end } end },
						Fire = function() end,
						Destroy = function() end,
					}
				end
				return roblox.Instance.new(className)
			end,
		},
		Vector3 = roblox.Vector3,
		CFrame = roblox.CFrame,
		Color3 = roblox.Color3,
		script = { Parent = setmetatable({}, { __index = function(_, name) return name end }) },
		os = os,
		task = {
			delay = function() end,
			defer = function(fn) fn() end,
		},
	}
	local function loadModule(name: string)
		if modules[name] then return modules[name] end
		local source = fs.readFile("tools/plugin_ws_bridge/" .. name .. ".module.luau"):gsub("\r\n", "\n")
		modules[name] = luau.load(source, { environment = env, debugName = name })()
		return modules[name]
	end
	env.require = loadModule
	local StudioChanges = luau.load(trackedSource, { environment = env, debugName = "BridgeStudioChanges" })()
	local tracker = StudioChanges.create({
		LUA_SOURCE_CLASS = { Script = true, LocalScript = true, ModuleScript = true },
		bridgeRuntimeId = "bulk-test-tracker",
		loadPendingStudioChanges = function() return nil end,
		savePendingStudioChanges = function() return nil end,
	}, { Workspace = true })
	tracker.observeExports(function() return true end, function(inst, prop)
		table.insert(exportEvents, inst.Name .. "." .. tostring(prop))
	end)

	local expected = { Transparency = { [part] = 0.5 } }
	tracker.setBulkExpectedProperties(expected)
	part.Transparency = 0.5
	itemChangedSignal:Fire(part, "Transparency")
	assert(#exportEvents == 0, "expected bulk write must be suppressed in ItemChanged fast path")
	assert(expected.Transparency[part] == nil, "matched bulk expectation must be consumed")

	part.Reflectance = 0.25
	itemChangedSignal:Fire(part, "Reflectance")
	assert(#exportEvents == 1 and exportEvents[1] == "BulkObserved.Reflectance", "outside property edit must not be suppressed")

	expected.Transparency[part] = 0.25
	part.Transparency = 0.9
	itemChangedSignal:Fire(part, "Transparency")
	assert(#exportEvents == 2 and exportEvents[2] == "BulkObserved.Transparency", "outside value mismatch must not be suppressed")

	tracker.setBulkExpectedProperties(nil)
	tracker.stop()
end


local function buildReviewHarness(opts: { [string]: any }?)
	opts = opts or {}
	local realGame = roblox.Instance.new("DataModel")
	local workspace = opts.workspace or realGame:GetService("Workspace")
	local serverStorage = opts.serverStorage or realGame:GetService("ServerStorage")
	local structureGen = opts.structureGen or { Workspace = 0, ServerStorage = 0 }
	local terrainDigest = opts.terrainDigest
	local activeBulkExpected = nil
	local activeBulkWrite = nil
	local fakeGame = {
		GetService = function(_, name)
			if name == "Workspace" then
				return workspace
			end
			if name == "ServerStorage" then
				return serverStorage
			end
			if name == "EncodingService" then
				return {
					Base64Encode = function(_, b) return buffer.fromstring(base64Encode(b)) end,
					Base64Decode = function(_, b) return base64Decode(b) end,
				}
			end
			if name == "HttpService" then
				return {
					GenerateGUID = function() return "00000000-0000-0000-0000-000000000000" end,
					JSONEncode = function(_, v) return tostring(v) end,
				}
			end
			if name == "ScriptEditorService" then
				return {
					FindScriptDocument = function() return nil end,
					GetScriptDocuments = function() return {} end,
				}
			end
			if name == "RunService" then
				return { Heartbeat = { Wait = function() end } }
			end
			if name == "CollectionService" then
				return {
					GetTags = function() return {} end,
					HasTag = function() return false end,
					AddTag = function() end,
					RemoveTag = function() end,
				}
			end
			if name == "ChangeHistoryService" then
				return {
					TryBeginRecording = function(_, recName) return recName end,
					FinishRecording = function() end,
				}
			end
			if name == "SerializationService" then
				return {}
			end
			if name == "Selection" then
				return { Get = function() return {} end, Set = function() end }
			end
			return realGame:GetService(name)
		end,
	}
	local env: { [string]: any } = {
		game = fakeGame,
		Enum = roblox.Enum,
		Instance = {
			new = function(className: string)
				if className == "BindableEvent" then
					return {
						Event = { Connect = function() return { Disconnect = function() end } end },
						Fire = function() end,
						Destroy = function() end,
					}
				end
				return roblox.Instance.new(className)
			end,
		},
		Vector3 = roblox.Vector3,
		CFrame = roblox.CFrame,
		Color3 = roblox.Color3,
		script = { Parent = setmetatable({}, { __index = function(_, name) return name end }) },
		os = opts.os or os,
		task = opts.task or {
			delay = function() end,
			defer = function(fn) fn() end,
		},
	}
	local modules: { [string]: any } = {
		RbxDom = {
			findCanonicalPropertyDescriptor = function() return nil end,
			getContentPropertyAliases = function() return {} end,
		},
	}
	local function loadModule(name: string)
		if modules[name] then return modules[name] end
		local source = fs.readFile("tools/plugin_ws_bridge/" .. name .. ".module.luau"):gsub("\r\n", "\n")
		modules[name] = luau.load(source, { environment = env, debugName = name })()
		return modules[name]
	end
	env.require = loadModule
	local EditorSync = loadModule("BridgeEditorSync")
	local stats = {
		requests = 0, lastMs = 0, lastAtUnix = 0, lastOk = true,
		propertyUpdated = 0, attributeUpdated = 0, noops = 0, errors = 0,
		sourceCreated = 0, sourceUpdated = 0, sourceDeleted = 0,
		instanceCreated = 0, instanceReplaced = 0, instanceDeleted = 0,
	}
	local ctx = {
		stats = stats,
		allowedServices = { Workspace = true, ServerStorage = true },
		maxChangesPerRequest = 5000,
		runtimeId = "bulk-review-session",
		serializeValue = function(v) return v end,
		assertSessionOwnership = function() end,
		captureSessionOwnership = function() return 1 end,
		currentSessionOwnership = function() return 1 end,
		withSessionOwnership = function(_, fn) return fn() end,
		studioChangeGeneration = function() return 0 end,
		nativeExportGeneration = function() return 0 end,
		structureGeneration = function(svc) return structureGen[svc] or 0 end,
		captureTerrainProof = if terrainDigest ~= nil then function() return terrainDigest end else nil,
		attributeObservation = function() return { active = true, generation = 0 } end,
		beginStudioChangeJournal = function() end,
		drainStudioChangeJournal = function() return {} end,
		retireStudioChangeJournal = function() end,
		expireStudioChangeJournal = function() end,
		finishStudioChangeJournal = function() return {} end,
		finishEditorTransactionExpectation = function() end,
		invalidateService = function() end,
		beginStudioChangeSuppression = function() end,
		endStudioChangeSuppression = function() end,
		expectParentChange = function() end,
		expectPropertyEvent = function() end,
		expectAttributeEvent = function() end,
		cancelExpectedEvent = function() end,
		samplePropertyChange = function() end,
		beginNativeImportObservations = function() end,
		finishNativeImportObservations = function() return {} end,
		updateStatus = function() end,
		setBulkExpectedProperties = function(expected, active)
			activeBulkExpected = expected
			activeBulkWrite = active
		end,
		noteBulkExportChange = function() end,
		afterBridgeExclusiveIdle = function(fn) fn() end,
	}
	return EditorSync.create(ctx), workspace, serverStorage, structureGen, function()
		return activeBulkExpected, activeBulkWrite
	end
end

do
	local api, workspace, _, structureGen = buildReviewHarness()
	local folder = roblox.Instance.new("Folder")
	folder.Name = "Sibs"
	folder.Parent = workspace
	local p1 = roblox.Instance.new("Part")
	p1.Name = "Part"
	p1.Transparency = 0
	p1.Parent = folder
	local p2 = roblox.Instance.new("Part")
	p2.Name = "Part"
	p2.Transparency = 0
	p2.Parent = folder
	local p3 = roblox.Instance.new("Part")
	p3.Name = "Part"
	p3.Transparency = 0
	p3.Parent = folder

	local n5ItemChanged: { (any, any) -> () } = {}
	local n5WsProxy: any = setmetatable({}, {
		__index = function(_, k)
			if k == "DescendantAdded" or k == "DescendantRemoving" or k == "AttributeChanged" or k == "Changed" then
				return { Connect = function() return { Disconnect = function() end } end }
			end
			if k == "GetAttributes" then return function() return {} end end
			if k == "GetDebugId" then return function() return "dbg-n5-ws" end end
			if k == "GetChildren" then return function() return {} end end
			if k == "GetDescendants" then return function() return {} end end
			if k == "IsDescendantOf" then return function() return true end end
			return (workspace :: any)[k]
		end,
	})
	local n5FakeGame: any = {
		ItemChanged = {
			Connect = function(_, cb)
				n5ItemChanged[#n5ItemChanged + 1] = cb
				return { Disconnect = function() end }
			end,
		},
		GetService = function(_, name: string)
			if name == "Workspace" then return n5WsProxy end
			if name == "CollectionService" then
				return {
					GetAllTags = function() return {} end,
					GetTagged = function() return {} end,
					GetTags = function() return {} end,
					TagAdded = { Connect = function() return { Disconnect = function() end } end },
					TagRemoved = { Connect = function() return { Disconnect = function() end } end },
					GetInstanceAddedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
					GetInstanceRemovedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
				}
			end
			if name == "RunService" then
				return { Heartbeat = { Connect = function() return { Disconnect = function() end } end } }
			end
			if name == "ScriptEditorService" then
				return {
					GetScriptDocuments = function() return {} end,
					FindScriptDocument = function() return nil end,
					TextDocumentDidChange = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidOpen = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidClose = { Connect = function() return { Disconnect = function() end } end },
				}
			end
			if name == "ChangeHistoryService" then
				return {
					TryBeginRecording = function() return nil end,
					FinishRecording = function() end,
				}
			end
			if name == "Selection" then
				return {
					Get = function() return {} end,
					Set = function() end,
				}
			end
			return { GetChildren = function() return {} end, GetDescendants = function() return {} end }
		end,
	}
	local n5Env: { [string]: any } = {
		game = n5FakeGame,
		typeof = function(v: any)
			if v == n5WsProxy then return "Instance" end
			return typeof(v)
		end,
		Enum = roblox.Enum,
		Instance = {
			new = function(className: string)
				if className == "BindableEvent" then
					return {
						Event = { Connect = function() return { Disconnect = function() end } end },
						Fire = function() end,
						Destroy = function() end,
					}
				end
				return roblox.Instance.new(className)
			end,
		},
		Vector3 = roblox.Vector3,
		CFrame = roblox.CFrame,
		Color3 = roblox.Color3,
		script = { Parent = setmetatable({}, { __index = function(_, name) return name end }) },
		os = os,
		task = { delay = function() end, defer = function(fn) fn() end },
	}
	local n5Modules: { [string]: any } = {
		RbxDom = {
			findCanonicalPropertyDescriptor = function(_, propName)
				return { name = propName, scriptability = "ReadWrite", dataType = "String" }
			end,
			getContentPropertyAliases = function() return {} end,
			getPropertyNames = function() return { "Name", "Parent", "Transparency" } end,
		},
	}
	local function loadN5(name: string)
		if n5Modules[name] then return n5Modules[name] end
		local src = fs.readFile("tools/plugin_ws_bridge/" .. name .. ".module.luau"):gsub("\r\n", "\n")
		n5Modules[name] = luau.load(src, { environment = n5Env, debugName = name })()
		return n5Modules[name]
	end
	n5Env.require = loadN5
	local n5Tracker = loadN5("BridgeStudioChanges").create({
		LUA_SOURCE_CLASS = { Script = true, LocalScript = true, ModuleScript = true },
		bridgeRole = "edit",
		bridgeRuntimeId = "n5-rename-tracker",
		shouldIgnoreInstance = function() return false end,
		invalidateRuntimeExportCache = function() end,
		loadPendingStudioChanges = function() return nil end,
		savePendingStudioChanges = function() return nil end,
	}, { Workspace = true })
	local prSource = fs.readFile("tools/plugin_ws_bridge/BridgePluginRuntime.module.luau"):gsub("\r\n", "\n")
	local prFirst = assert(prSource:find("local nativeStructureGenerationByService:", 1, true))
	local prLast = assert(prSource:find("function Config.updateStatusText()", prFirst, true))
	local prConfig = {
		studioChanges = setmetatable({
			trackedServiceRoot = function(inst: any)
				if inst == workspace or (typeof(inst) == "Instance" and inst:IsDescendantOf(workspace)) then
					return workspace
				end
				return n5Tracker.trackedServiceRoot(inst)
			end,
		}, { __index = n5Tracker }),
	}
	local prGetStructureGen = luau.load(
		prSource:sub(prFirst, prLast - 1)
			.. "\nreturn function(svc) return nativeStructureGenerationByService[svc] end",
		{
			environment = {
				Config = prConfig,
				PropertySchemaModule = {
					makeExportPropertyFilter = function()
						return function(_, propName: string)
							return propName == "Name" or propName == "Parent" or propName == "Archivable"
						end
					end,
				},
				ALLOWED_SERVICES = { Workspace = true },
				game = setmetatable({
					GetService = function(_, name: string)
						if name == "Workspace" then return workspace end
						return n5FakeGame:GetService(name)
					end,
				}, {}),
				typeof = function(v: any)
					if v == n5WsProxy then return "Instance" end
					return typeof(v)
				end,
				includeExportInstance = function() return true end,
				string = string,
				tostring = tostring,
				pairs = pairs,
				type = type,
			},
			debugName = "BridgePluginRuntime-N5",
		}
	)()
	n5Tracker.getState({ start = true, trackingGuardId = "n5-guard", services = { "Workspace" }, syncbackProperties = true })
	local function syncN5StructureGen()
		structureGen.Workspace = prGetStructureGen("Workspace")
	end
	syncN5StructureGen()

	local stringsB64 = base64Encode(buffer.fromstring("Sibs\0Folder\0Part"))
	local nodesOnlyP2 = packNodes({ { 0, 1, 0, 1 }, { 1, 2, 2, 2 } })
	api.withRequestLease("l-rename-1", false, api.beginTransaction, {
		transactionId = "tx-rename-1",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local r1 = api.withRequestLease("l-rename-1", false, api.applyBulkProperties, {
		transactionId = "tx-rename-1",
		service = "Workspace",
		tree = {
			strings = stringsB64,
			stringCount = 3,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			cacheKey = "sibs-only-p2",
			nodes = nodesOnlyP2,
		},
		columns = {
			{ propertyName = "Transparency", encoding = "f32_const", count = 1, rows = packU32({ 2 }), value = 0.25 },
		},
	})
	assert(r1.treeCacheHit ~= true, "first push must populate cache")
	assert(math.abs(p2.Transparency - 0.25) < 1e-5, "Part[2] before rename must target p2")
	api.withRequestLease("l-rename-1", false, api.commitTransaction, { transactionId = "tx-rename-1" })

	api.withRequestLease("l-rename-hit", false, api.beginTransaction, {
		transactionId = "tx-rename-hit",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local rHit = api.withRequestLease("l-rename-hit", false, api.applyBulkProperties, {
		transactionId = "tx-rename-hit",
		service = "Workspace",
		tree = {
			strings = stringsB64,
			stringCount = 3,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			cacheKey = "sibs-only-p2",
			nodes = nodesOnlyP2,
		},
		columns = {
			{ propertyName = "Transparency", encoding = "f32_const", count = 1, rows = packU32({ 2 }), value = 0.4 },
		},
	})
	assert(rHit.treeCacheHit == true, "unchanged second push with same cacheKey must hit persistentBulkTreeCache")
	assert(math.abs(p2.Transparency - 0.4) < 1e-5, "same-key cache hit must resolve p2")
	api.withRequestLease("l-rename-hit", false, api.commitTransaction, { transactionId = "tx-rename-hit" })

	local genBeforeRename = prGetStructureGen("Workspace")
	p1.Name = "Renamed"
	for _, cb in ipairs(table.clone(n5ItemChanged)) do
		cb(p1, "Name")
	end
	syncN5StructureGen()
	assert(structureGen.Workspace > genBeforeRename, "N5: ItemChanged(Name) through real BridgePluginRuntime must bump nativeStructureGenerationByService")

	api.withRequestLease("l-rename-2", false, api.beginTransaction, {
		transactionId = "tx-rename-2",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local r2 = api.withRequestLease("l-rename-2", false, api.applyBulkProperties, {
		transactionId = "tx-rename-2",
		service = "Workspace",
		tree = {
			strings = stringsB64,
			stringCount = 3,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			cacheKey = "sibs-only-p2",
			nodes = nodesOnlyP2,
		},
		columns = {
			{ propertyName = "Transparency", encoding = "f32_const", count = 1, rows = packU32({ 2 }), value = 0.75 },
		},
	})
	assert(r2.treeCacheHit ~= true, "N5: rename must invalidate persistentBulkTreeCache even for same cacheKey and same totalNodes")
	api.withRequestLease("l-rename-2", false, api.commitTransaction, { transactionId = "tx-rename-2" })
	assert(math.abs(p3.Transparency - 0.75) < 1e-5, "N5: Part[2] after rename must target p3")
	assert(math.abs(p2.Transparency - 0.4) < 1e-5, "N5: p2 (now Part[1]) must stay untouched at 0.4")
	n5Tracker.stop()
end

do
	local function makeFakePart(name: string, parent: any, initialTransparency: number, rejectWrite: boolean)
		local props: { [string]: any } = {
			Name = name,
			ClassName = "Part",
			Parent = parent,
			Transparency = initialTransparency,
		}
		local obj: any
		obj = setmetatable({}, {
			__index = function(_, k)
				if k == 1 then return obj end
				if k == "GetChildren" then return function() return {} end end
				if k == "GetFullName" then return function() return "Workspace." .. props.Name end end
				if k == "IsA" then return function(_, c) return c == "Part" or c == "BasePart" end end
				if k == "IsDescendantOf" then return function(_, p) return p == props.Parent end end
				return props[k]
			end,
			__newindex = function(_, k, v)
				if k == "Transparency" and rejectWrite then
					return -- Studio did not retain
				end
				props[k] = v
			end,
		})
		return obj
	end
	local children = {}
	local fakeWs: any = {
		Name = "Workspace",
		ClassName = "Workspace",
		GetChildren = function() return children end,
		FindFirstChildOfClass = function() return nil end,
		GetFullName = function() return "Workspace" end,
	}
	local goodPart = makeFakePart("P1", fakeWs, 0.1, false)
	local badPart = makeFakePart("P2", fakeWs, 0.2, true)
	table.insert(children, goodPart)
	table.insert(children, badPart)

	local api = buildReviewHarness({ workspace = fakeWs })
	api.withRequestLease("l-mid", false, api.beginTransaction, {
		transactionId = "tx-mid",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local stringsB64 = base64Encode(buffer.fromstring("P1\0Part\0P2"))
	local ok, err = pcall(api.withRequestLease, "l-mid", false, api.applyBulkProperties, {
		transactionId = "tx-mid",
		service = "Workspace",
		tree = {
			strings = stringsB64,
			stringCount = 3,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			nodes = packNodes({ { 0, 1, 0, 1 }, { 0, 1, 2, 1 } }),
		},
		columns = {
			{ propertyName = "Transparency", encoding = "f32_const", count = 2, rows = packU32({ 1, 2 }), value = 0.8 },
		},
	})
	assert(not ok and string.find(tostring(err), "did not retain", 1, true) ~= nil, "expected retention failure mid-column")
	assert(math.abs(goodPart.Transparency - 0.8) < 1e-5, "P1 was written before P2 failed")
	local rb = api.withRequestLease("l-mid", false, api.rollbackTransaction, { transactionId = "tx-mid" })
	assert(rb.ok == true and rb.state == "rolledBack")
	assert(math.abs(goodPart.Transparency - 0.1) < 1e-5, "P1 must be restored by rollback after mid-column error")
end

do
	local function makeTruncatingPart(name: string, parent: any)
		local props: { [string]: any } = {
			Name = name,
			ClassName = "Part",
			Parent = parent,
			Color = roblox.Color3.new(0, 0, 0),
			Size = roblox.Vector3.new(4, 1, 2),
			CFrame = roblox.CFrame.new(0, 0, 0),
		}
		local obj: any
		obj = setmetatable({}, {
			__index = function(_, k)
				if k == 1 then return obj end
				if k == "GetChildren" then return function() return {} end end
				if k == "GetFullName" then return function() return "Workspace." .. props.Name end end
				if k == "IsA" then return function(_, c) return c == "Part" or c == "BasePart" end end
				if k == "IsDescendantOf" then return function(_, p) return p == props.Parent end end
				return props[k]
			end,
			__newindex = function(_, k, v)
				if k == "Color" then
					props.Color = roblox.Color3.new(
						math.floor(v.R * 255) / 255,
						math.floor(v.G * 255) / 255,
						math.floor(v.B * 255) / 255
					)
				else
					props[k] = v
				end
			end,
		})
		return obj
	end
	local children = {}
	local fakeWs: any = {
		Name = "Workspace",
		ClassName = "Workspace",
		GetChildren = function() return children end,
		FindFirstChildOfClass = function() return nil end,
		GetFullName = function() return "Workspace" end,
	}
	local bp = makeTruncatingPart("BytePart", fakeWs)
	local light = roblox.Instance.new("PointLight")
	light.Name = "FloatLight"
	light.Color = roblox.Color3.new(0.5, 0.5, 0.5)
	table.insert(children, bp)
	table.insert(children, light)

	local api = buildReviewHarness({ workspace = fakeWs })
	api.withRequestLease("l-eq", false, api.beginTransaction, {
		transactionId = "tx-eq",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local stringsB64 = base64Encode(buffer.fromstring("BytePart\0Part\0FloatLight\0PointLight"))
	local cfBuf = buffer.create(48)
	local angle = 5e-5
	local c, s = math.cos(angle), math.sin(angle)
	local comps = { 0, 0, 0, c, -s, 0, s, c, 0, 0, 0, 1 }
	for i, v in ipairs(comps) do
		buffer.writef32(cfBuf, (i - 1) * 4, v)
	end
	local res = api.withRequestLease("l-eq", false, api.applyBulkProperties, {
		transactionId = "tx-eq",
		service = "Workspace",
		tree = {
			strings = stringsB64,
			stringCount = 4,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			nodes = packNodes({ { 0, 1, 0, 1 }, { 0, 1, 2, 3 } }),
		},
		columns = {
			{ propertyName = "Color", encoding = "color3", count = 2, rows = packU32({ 1, 2 }), values = packVec3({ { 0.5, 0.5, 0.5 }, { 0.50005, 0.5, 0.5 } }) },
			{ propertyName = "Size", encoding = "vector3", count = 1, rows = packU32({ 1 }), values = packVec3({ { 4.00005, 1, 2 } }) },
			{ propertyName = "CFrame", encoding = "cframe", count = 1, rows = packU32({ 1 }), values = base64Encode(cfBuf) },
		},
	})
	assert(res.ok == true and res.verified == 4 and res.propertyUpdated == 4, "all 4 edits (byte color, sub-1e-4 float color, sub-1e-4 size, sub-1e-4 rotation) must be written & verified")
	assert(res.exactRetained == false, "byte-truncated BasePart.Color=0.5 must mark exactRetained=false")
	api.withRequestLease("l-eq", false, api.commitTransaction, { transactionId = "tx-eq" })
end

do
	local api, workspace = buildReviewHarness()
	local part = roblox.Instance.new("Part")
	part.Name = "BigAttrPart"
	part:SetAttribute("BigId", 4023948573)
	part.Parent = workspace

	api.withRequestLease("l-attr5", false, api.beginTransaction, {
		transactionId = "tx-attr5",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local stringsB64 = base64Encode(buffer.fromstring("BigAttrPart\0Part"))
	local res = api.withRequestLease("l-attr5", false, api.applyBulkProperties, {
		transactionId = "tx-attr5",
		service = "Workspace",
		tree = {
			strings = stringsB64,
			stringCount = 2,
			nodeOffset = 0,
			nodeCount = 1,
			totalNodes = 1,
			nodes = packNodes({ { 0, 1, 0, 1 } }),
		},
		columns = {
			{
				kind = "attribute",
				propertyName = "BigId",
				encoding = "indexed",
				count = 1,
				rows = packU32({ 1 }),
				palette = { 4023948571 },
				indices = packU32({ 0 }),
			},
		},
	})
	assert(res.ok == true and res.propertyUpdated == 1 and res.noops == 0, "4023948573 vs 4023948571 must not match as noop")
	assert(part:GetAttribute("BigId") == 4023948571, "exact attribute integer must be written")
	api.withRequestLease("l-attr5", false, api.commitTransaction, { transactionId = "tx-attr5" })
end

do
	local part = roblox.Instance.new("Part")
	part.Name = "Leaf"
	part.Size = roblox.Vector3.new(8, 2, 4)
	local model: any
	model = {
		Name = "ScaledModel",
		ClassName = "Model",
		WorldPivot = roblox.CFrame.new(10, 0, 0),
		PrimaryPart = nil,
		GetScale = function() return 2 end,
		GetChildren = function() return { part } end,
		GetFullName = function() return "Workspace.ScaledModel" end,
	}
	model[1] = model
	local fakeWs: any = {
		Name = "Workspace",
		ClassName = "Workspace",
		GetChildren = function() return { model } end,
		FindFirstChildOfClass = function() return nil end,
		GetFullName = function() return "Workspace" end,
	}
	local api = buildReviewHarness({ workspace = fakeWs })
	api.withRequestLease("l-scale", false, api.beginTransaction, {
		transactionId = "tx-scale",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local stringsB64 = base64Encode(buffer.fromstring("ScaledModel\0Model\0Leaf\0Part"))
	local res = api.withRequestLease("l-scale", false, api.applyBulkProperties, {
		transactionId = "tx-scale",
		service = "Workspace",
		tree = {
			strings = stringsB64,
			stringCount = 4,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			nodes = packNodes({ { 0, 1, 0, 1 }, { 1, 1, 2, 3 } }),
		},
		columns = {
			{ kind = "assert_model_scale", propertyName = "Scale", encoding = "f32_const", count = 1, rows = packU32({ 1 }), value = 1.0 },
			{ propertyName = "Size", encoding = "vector3", count = 1, rows = packU32({ 2 }), values = packVec3({ { 4, 1, 2 } }) },
		},
	})
	assert(res.fallbackFullCapture == true and res.fallbackReason == "model_scale", "Model scale mismatch must fall back")
	assert(part.Size == roblox.Vector3.new(8, 2, 4), "assert_model_scale fallback must happen before mutating parts")
	api.withRequestLease("l-scale", false, api.rollbackTransaction, { transactionId = "tx-scale" })
end

do
	local attrs: { [string]: any } = { RBX_Engine = 1, User = "old", NanMetric = 0 / 0 }
	local fakePart: any
	local fakeWs: any
	fakePart = setmetatable({}, {
		__index = function(_, k)
			if k == 1 then return fakePart end
			if k == "Name" then return "AttrPart" end
			if k == "ClassName" then return "Part" end
			if k == "Parent" then return fakeWs end
			if k == "GetChildren" then return function() return {} end end
			if k == "GetFullName" then return function() return "Workspace.AttrPart" end end
			if k == "GetAttribute" then return function(_, ak) return attrs[ak] end end
			if k == "GetAttributes" then return function() return table.clone(attrs) end end
			if k == "SetAttribute" then
				return function(_, ak, av)
					if string.sub(tostring(ak), 1, 4) == "RBX_" then
						error("cannot write RBX_ attribute: " .. tostring(ak))
					end
					attrs[ak] = av
				end
			end
			return nil
		end,
	})
	fakeWs = {
		Name = "Workspace",
		ClassName = "Workspace",
		GetChildren = function() return { fakePart } end,
		FindFirstChildOfClass = function() return nil end,
		GetFullName = function() return "Workspace" end,
	}
	local api = buildReviewHarness({ workspace = fakeWs })
	api.withRequestLease("l-rbx", false, api.beginTransaction, {
		transactionId = "tx-rbx",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local stringsB64 = base64Encode(buffer.fromstring("AttrPart\0Part"))
	local res = api.withRequestLease("l-rbx", false, api.applyBulkProperties, {
		transactionId = "tx-rbx",
		service = "Workspace",
		returnOldValues = true,
		tree = {
			strings = stringsB64,
			stringCount = 2,
			nodeOffset = 0,
			nodeCount = 1,
			totalNodes = 1,
			nodes = packNodes({ { 0, 1, 0, 1 } }),
		},
		columns = {
			{ kind = "attribute", propertyName = "RBX_Engine", encoding = "indexed", count = 1, rows = packU32({ 1 }), palette = { 2 }, indices = packU32({ 0 }) },
			{ kind = "attribute", propertyName = "NanMetric", encoding = "indexed", count = 1, rows = packU32({ 1 }), palette = { 42 }, indices = packU32({ 0 }) },
			{ kind = "attributes_replace", propertyName = "@attributes", encoding = "indexed", count = 1, rows = packU32({ 1 }), palette = { { User = "new", NanMetric = 42, RBX_Other = 9 } }, indices = packU32({ 0 }) },
		},
	})
	assert(res.ok == true and attrs.RBX_Engine == 1 and attrs.RBX_Other == nil and attrs.User == "new" and attrs.NanMetric == 42)
	local rb = api.withRequestLease("l-rbx", false, api.rollbackTransaction, { transactionId = "tx-rbx" })
	assert(rb.ok == true and attrs.RBX_Engine == 1 and attrs.User == "old" and attrs.NanMetric ~= attrs.NanMetric, "rollback must skip RBX_ and restore NaN/User")
end

do
	local api, workspace, serverStorage = buildReviewHarness({ terrainDigest = "actual-digest" })
	local ssPart = roblox.Instance.new("Part")
	ssPart.Name = "SSPart"
	ssPart.Transparency = 0.2
	ssPart.Parent = serverStorage
	local wsPart = roblox.Instance.new("Part")
	wsPart.Name = "WSPart"
	wsPart.Transparency = 0.3
	wsPart.Parent = workspace

	api.withRequestLease("l-terr8", false, api.beginTransaction, {
		transactionId = "tx-terr8",
		services = { "ServerStorage", "Workspace" },
		hasBulkPropertyPatches = true,
	})
	api.withRequestLease("l-terr8", false, api.applyBulkProperties, {
		transactionId = "tx-terr8",
		service = "ServerStorage",
		tree = {
			strings = base64Encode(buffer.fromstring("SSPart\0Part")),
			stringCount = 2,
			nodeOffset = 0,
			nodeCount = 1,
			totalNodes = 1,
			nodes = packNodes({ { 0, 1, 0, 1 } }),
		},
		columns = {
			{ propertyName = "Transparency", encoding = "f32_const", count = 1, rows = packU32({ 1 }), value = 0.9 },
		},
	})
	assert(math.abs(ssPart.Transparency - 0.9) < 1e-5)
	local wsRes = api.withRequestLease("l-terr8", false, api.applyBulkProperties, {
		transactionId = "tx-terr8",
		service = "Workspace",
		expectedTerrainDigest = "different-digest",
		tree = {
			strings = base64Encode(buffer.fromstring("WSPart\0Part")),
			stringCount = 2,
			nodeOffset = 0,
			nodeCount = 1,
			totalNodes = 1,
			nodes = packNodes({ { 0, 1, 0, 1 } }),
		},
		columns = {
			{ propertyName = "Transparency", encoding = "f32_const", count = 1, rows = packU32({ 1 }), value = 0.8 },
		},
	})
	assert(wsRes.fallbackFullCapture == true and wsRes.fallbackReason == "terrain_digest")
	local rb = api.withRequestLease("l-terr8", false, api.rollbackTransaction, { transactionId = "tx-terr8" })
	assert(rb.ok == true and rb.state == "rolledBack")
	assert(math.abs(ssPart.Transparency - 0.2) < 1e-5, "ServerStorage mutation must still roll back after Workspace terrain fallback")
end

do
	local api, workspace = buildReviewHarness()
	local p1 = roblox.Instance.new("Part")
	p1.Name = "ChunkA"
	p1.Parent = workspace
	local p2 = roblox.Instance.new("Part")
	p2.Name = "ChunkB"
	p2.Parent = workspace
	local stringsB64 = base64Encode(buffer.fromstring("ChunkA\0Part\0ChunkB"))

	for round = 1, 2 do
		local txId = "tx-chunk-" .. tostring(round)
		api.withRequestLease("l-chunk", false, api.beginTransaction, {
			transactionId = txId,
			services = { "Workspace" },
			hasBulkPropertyPatches = true,
		})
		local c0 = api.withRequestLease("l-chunk", false, api.applyBulkProperties, {
			transactionId = txId,
			service = "Workspace",
			tree = {
				strings = stringsB64,
				stringCount = 3,
				nodeOffset = 0,
				nodeCount = 1,
				totalNodes = 2,
				cacheKey = "chunked-key",
				nodes = packNodes({ { 0, 1, 0, 1 } }),
			},
			columns = {},
		})
		if round == 2 then
			assert(c0.treeCacheHit == true, "second push chunk 0 must report treeCacheHit=true")
		end
		local c1 = api.withRequestLease("l-chunk", false, api.applyBulkProperties, {
			transactionId = txId,
			service = "Workspace",
			tree = {
				nodeOffset = 1,
				nodeCount = 1,
				totalNodes = 2,
				nodes = packNodes({ { 0, 1, 2, 1 } }),
			},
			columns = {
				{ propertyName = "Transparency", encoding = "f32_const", count = 2, rows = packU32({ 1, 2 }), value = 0.25 * round },
			},
		})
		assert(c1.ok == true and c1.verified == 2, "chunk 1 after cache hit must succeed")
		api.withRequestLease("l-chunk", false, api.commitTransaction, { transactionId = txId })
	end
	assert(math.abs(p2.Transparency - 0.5) < 1e-5)
end

do
	local attrCallbacks: { [any]: (string) -> () } = {}
	local realGame = roblox.Instance.new("DataModel")
	local workspace = realGame:GetService("Workspace")
	local attrs: { [string]: any } = { A = 1, B = 0 }
	local fakePart: any
	local fakeWs: any
	fakePart = setmetatable({}, {
		__index = function(_, k)
			if k == "Name" then return "AttrObs" end
			if k == "ClassName" then return "Part" end
			if k == "Parent" then return fakeWs end
			if k == "Archivable" then return true end
			if k == "GetChildren" then return function() return {} end end
			if k == "GetDescendants" then return function() return {} end end
			if k == "GetFullName" then return function() return "Workspace.AttrObs" end end
			if k == "IsA" then return function(_, c) return c == "Part" or c == "BasePart" end end
			if k == "IsDescendantOf" then return function(_, p) return p == fakeWs end end
			if k == "GetAttribute" then return function(_, ak) return attrs[ak] end end
			if k == "GetAttributes" then return function() return table.clone(attrs) end end
			if k == "AttributeChanged" then
				return {
					Connect = function(_, cb)
						attrCallbacks[cb] = cb
						return { Disconnect = function() attrCallbacks[cb] = nil end }
					end,
				}
			end
			if k == "Changed" then
				return { Connect = function() return { Disconnect = function() end } end }
			end
			return nil
		end,
	})
	fakeWs = {
		ClassName = "Workspace",
		Name = "Workspace",
		Archivable = true,
		DescendantAdded = { Connect = function() return { Disconnect = function() end } end },
		DescendantRemoving = { Connect = function() return { Disconnect = function() end } end },
		Changed = { Connect = function() return { Disconnect = function() end } end },
		AttributeChanged = { Connect = function() return { Disconnect = function() end } end },
		GetChildren = function() return { fakePart } end,
		GetDescendants = function() return { fakePart } end,
		GetFullName = function() return "Workspace" end,
		GetAttributes = function() return {} end,
		IsDescendantOf = function() return false end,
		IsA = function(_, c) return c == "Workspace" end,
	}
	local fakeGame = {
		ItemChanged = { Connect = function() return { Disconnect = function() end } end },
		GetService = function(_, name)
			if name == "Workspace" then return fakeWs end
			if name == "EncodingService" then
				return {
					Base64Encode = function(_, b) return buffer.fromstring(base64Encode(b)) end,
					Base64Decode = function(_, b) return base64Decode(b) end,
				}
			end
			if name == "CollectionService" then
				return {
					GetTags = function() return {} end,
					GetAllTags = function() return {} end,
					GetTagged = function() return {} end,
					TagAdded = { Connect = function() return { Disconnect = function() end } end },
					TagRemoved = { Connect = function() return { Disconnect = function() end } end },
					GetInstanceAddedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
					GetInstanceRemovedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
				}
			end
			if name == "RunService" then
				return { Heartbeat = { Connect = function() return { Disconnect = function() end } end } }
			end
			if name == "HttpService" then
				return {
					GenerateGUID = function() return "00000000-0000-0000-0000-000000000000" end,
					JSONEncode = function(_, v) return tostring(v) end,
				}
			end
			if name == "ScriptEditorService" then
				return {
					GetScriptDocuments = function() return {} end,
					FindScriptDocument = function() return nil end,
					TextDocumentDidChange = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidOpen = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidClose = { Connect = function() return { Disconnect = function() end } end },
				}
			end
			if name == "ChangeHistoryService" then
				return {
					TryBeginRecording = function() return nil end,
					FinishRecording = function() end,
				}
			end
			if name == "Selection" then
				return {
					Get = function() return {} end,
					Set = function() end,
				}
			end
			return realGame:GetService(name)
		end,
	}
	fakeWs.Parent = fakeGame
	local env: { [string]: any } = {
		game = fakeGame,
		Enum = roblox.Enum,
		Instance = {
			new = function(className: string)
				if className == "BindableEvent" then
					return {
						Event = { Connect = function() return { Disconnect = function() end } end },
						Fire = function() end,
						Destroy = function() end,
					}
				end
				return roblox.Instance.new(className)
			end,
		},
		Vector3 = roblox.Vector3,
		CFrame = roblox.CFrame,
		Color3 = roblox.Color3,
		script = { Parent = setmetatable({}, { __index = function(_, name) return name end }) },
		os = os,
		task = { delay = function() end, defer = function(fn) fn() end },
	}
	local modules: { [string]: any } = {
		RbxDom = {
			findCanonicalPropertyDescriptor = function() return nil end,
			getContentPropertyAliases = function() return {} end,
			getPropertyNames = function() return { "Transparency" } end,
		},
	}
	local function loadModule(name: string)
		if modules[name] then return modules[name] end
		local source = fs.readFile("tools/plugin_ws_bridge/" .. name .. ".module.luau"):gsub("\r\n", "\n")
		modules[name] = luau.load(source, { environment = env, debugName = name })()
		return modules[name]
	end
	env.require = loadModule
	local StudioChanges = loadModule("BridgeStudioChanges")
	local tracker = StudioChanges.create({
		LUA_SOURCE_CLASS = { Script = true, LocalScript = true, ModuleScript = true },
		bridgeRole = "edit",
		bridgeRuntimeId = "bulk-attr-tracker",
		shouldIgnoreInstance = function() return false end,
		invalidateRuntimeExportCache = function() end,
		loadPendingStudioChanges = function() return nil end,
		savePendingStudioChanges = function() return nil end,
	}, { Workspace = true })
	tracker.getState({ start = true, trackingGuardId = "attr-test-guard", services = { "Workspace" }, syncbackProperties = true })
	assert(next(attrCallbacks) ~= nil, "AttributeChanged must be connected for fakePart")
	local obs = tracker.attributeObservation("Workspace")
	assert(obs ~= nil, "attributeObservation must be active")
	tracker.beginChangeJournal("tx-attr-item10", { "Workspace" })
	local nilSent = {}
	tracker.setBulkExpectedProperties({
		["@attributes"] = {
			[fakePart] = { keys = { A = 1 }, relayPending = 0, relaySnapshot = { A = 1, B = 0 }, nilSentinel = nilSent },
		},
	})
	local function fireAttr(name: string)
		for _, cb in pairs(table.clone(attrCallbacks)) do
			cb(name)
		end
	end
	local beforeGen = obs.generation
	fireAttr("A")
	assert(obs.generation == beforeGen, `expected attribute A=1 must be suppressed: gen={obs.generation} before={beforeGen}`)
	attrs.B = 99
	fireAttr("B")
	assert(obs.generation == beforeGen + 1 and obs.attribute == "B", `outside attribute B must not be suppressed: gen={obs.generation} attr={tostring(obs.attribute)}`)
	local afterB = obs.generation
	attrs.A = 2
	fireAttr("A")
	assert(obs.generation == afterB + 1 and obs.attribute == "A", `outside edit to A=2 must not be suppressed: gen={obs.generation} attr={tostring(obs.attribute)}`)
	tracker.finishChangeJournal("tx-attr-item10")
	tracker.setBulkExpectedProperties(nil)
	tracker.stop()
end

do
	local itemChangedListeners: { (any, string) -> () } = {}
	local realGame = roblox.Instance.new("DataModel")
	local fakeCoreGui = realGame:GetService("CoreGui")
	local notifyChangedCbs: { (any) -> () } = {}
	local relayNotify: any = nil
	local relayAttrs: { [string]: any } = { A = 1, B = 0, C = 0 }

	local fakeGame: any
	local fakeWs: any
	local fakeRepStorage: any
	local function makeFakePart(name: string, parent: any, attrs: { [string]: any }?)
		local stateMap: { [string]: any } = {
			Name = name,
			ClassName = "Part",
			Parent = parent,
			Archivable = true,
			Transparency = 0,
			AssemblyLinearVelocity = roblox.Vector3.new(0, 0, 0),
		}
		local attrTable = attrs or {}
		local obj: any
		obj = setmetatable({}, {
			__index = function(_, k)
				if k == "GetChildren" then return function() return {} end end
				if k == "GetDescendants" then return function() return {} end end
				if k == "GetFullName" then return function() return tostring(stateMap.Parent and stateMap.Parent.Name or "") .. "." .. stateMap.Name end end
				if k == "IsA" then return function(_, c) return c == "Part" or c == "BasePart" end end
				if k == "IsDescendantOf" then return function(_, p) return p == stateMap.Parent or p == fakeGame end end
				if k == "GetAttribute" then return function(_, ak) return attrTable[ak] end end
				if k == "GetAttributes" then return function() return table.clone(attrTable) end end
				if k == "SetAttribute" then return function(_, ak, av) attrTable[ak] = av end end
				if k == "AttributeChanged" then return { Connect = function() return { Disconnect = function() end } end } end
				if k == "Changed" then return { Connect = function() return { Disconnect = function() end } end } end
				return stateMap[k]
			end,
			__newindex = function(_, k, v)
				stateMap[k] = v
			end,
		})
		return obj
	end

	fakeWs = {
		ClassName = "Workspace",
		Name = "Workspace",
		Archivable = true,
		DescendantAdded = { Connect = function() return { Disconnect = function() end } end },
		DescendantRemoving = { Connect = function() return { Disconnect = function() end } end },
		Changed = { Connect = function() return { Disconnect = function() end } end },
		AttributeChanged = { Connect = function() return { Disconnect = function() end } end },
		GetChildren = function() return {} end,
		GetDescendants = function() return {} end,
		GetFullName = function() return "Workspace" end,
		GetAttributes = function() return {} end,
		IsDescendantOf = function(_, p) return p == fakeGame end,
		IsA = function(_, c) return c == "Workspace" end,
	}
	fakeRepStorage = {
		ClassName = "ReplicatedStorage",
		Name = "ReplicatedStorage",
		Archivable = true,
		DescendantAdded = { Connect = function() return { Disconnect = function() end } end },
		DescendantRemoving = { Connect = function() return { Disconnect = function() end } end },
		Changed = { Connect = function() return { Disconnect = function() end } end },
		AttributeChanged = { Connect = function() return { Disconnect = function() end } end },
		GetChildren = function() return {} end,
		GetDescendants = function() return {} end,
		GetFullName = function() return "ReplicatedStorage" end,
		GetAttributes = function() return {} end,
		IsDescendantOf = function(_, p) return p == fakeGame end,
		IsA = function(_, c) return c == "ReplicatedStorage" end,
	}
	local lazyPart = makeFakePart("LazyPart", fakeWs)
	local otherPart = makeFakePart("OtherPart", fakeWs)
	local relayPart = makeFakePart("RelayPart", fakeWs, relayAttrs)
	fakeWs.GetChildren = function() return { lazyPart, otherPart, relayPart } end
	fakeWs.GetDescendants = function() return { lazyPart, otherPart, relayPart } end

	fakeGame = {
		ItemChanged = {
			Connect = function(_, cb)
				itemChangedListeners[#itemChangedListeners + 1] = cb
				return {
					Disconnect = function()
						for i, f in ipairs(itemChangedListeners) do
							if f == cb then
								table.remove(itemChangedListeners, i)
								break
							end
						end
					end,
				}
			end,
		},
		GetService = function(_, name)
			if name == "Workspace" then return fakeWs end
			if name == "ReplicatedStorage" then return fakeRepStorage end
			if name == "CoreGui" then return fakeCoreGui end
			if name == "EncodingService" then
				return {
					Base64Encode = function(_, b) return buffer.fromstring(base64Encode(b)) end,
					Base64Decode = function(_, b) return base64Decode(b) end,
				}
			end
			if name == "CollectionService" then
				return {
					GetTags = function() return {} end,
					GetAllTags = function() return {} end,
					GetTagged = function() return {} end,
					TagAdded = { Connect = function() return { Disconnect = function() end } end },
					TagRemoved = { Connect = function() return { Disconnect = function() end } end },
					GetInstanceAddedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
					GetInstanceRemovedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
				}
			end
			if name == "RunService" then
				return { Heartbeat = { Connect = function() return { Disconnect = function() end } end } }
			end
			if name == "HttpService" then
				return {
					GenerateGUID = function() return "11111111-1111-1111-1111-111111111111" end,
					JSONEncode = function(_, v) return tostring(v) end,
				}
			end
			if name == "ScriptEditorService" then
				return {
					GetScriptDocuments = function() return {} end,
					FindScriptDocument = function() return nil end,
					TextDocumentDidChange = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidOpen = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidClose = { Connect = function() return { Disconnect = function() end } end },
				}
			end
			if name == "ChangeHistoryService" then
				return {
					TryBeginRecording = function() return nil end,
					FinishRecording = function() end,
				}
			end
			if name == "Selection" then
				return {
					Get = function() return {} end,
					Set = function() end,
				}
			end
			return realGame:GetService(name)
		end,
	}
	fakeWs.Parent = fakeGame
	fakeRepStorage.Parent = fakeGame
	local env: { [string]: any } = {
		game = fakeGame,
		typeof = function(v: any)
			if v == lazyPart or v == otherPart or v == relayPart or v == fakeWs or v == fakeRepStorage then
				return "Instance"
			end
			return typeof(v)
		end,
		Enum = roblox.Enum,
		Instance = {
			new = function(className: string)
				if className == "BindableEvent" then
					return {
						Event = { Connect = function() return { Disconnect = function() end } end },
						Fire = function() end,
						Destroy = function() end,
					}
				end
				if className == "ObjectValue" then
					local obj: any = {
						Name = "ObjectValue",
						Archivable = true,
						Changed = {
							Connect = function(_, cb)
								notifyChangedCbs[#notifyChangedCbs + 1] = cb
								return { Disconnect = function() end }
							end,
						},
						SetAttribute = function() end,
						Destroy = function() end,
					}
					relayNotify = obj
					return obj
				end
				return roblox.Instance.new(className)
			end,
		},
		Vector3 = roblox.Vector3,
		CFrame = roblox.CFrame,
		Color3 = roblox.Color3,
		script = { Parent = setmetatable({}, { __index = function(_, name) return name end }) },
		os = os,
		task = { delay = function() end, defer = function(fn) fn() end },
	}
	local modules: { [string]: any } = {
		RbxDom = {
			findCanonicalPropertyDescriptor = function(_, propName)
				if
					propName == "Transparency"
					or propName == "Name"
					or propName == "Reflectance"
					or propName == "Position"
					or propName == "Mass"
				then
					return { name = propName, scriptability = "ReadWrite", dataType = "Float32" }
				end
				return nil
			end,
			getContentPropertyAliases = function() return {} end,
			getPropertyNames = function()
				return { "Transparency", "Name", "Reflectance", "Position", "Mass" }
			end,
		},
	}
	local function loadModule(name: string)
		if modules[name] then return modules[name] end
		local source = fs.readFile("tools/plugin_ws_bridge/" .. name .. ".module.luau"):gsub("\r\n", "\n")
		modules[name] = luau.load(source, { environment = env, debugName = name })()
		return modules[name]
	end
	env.require = loadModule
	local StudioChanges = loadModule("BridgeStudioChanges")
	local tracker = StudioChanges.create({
		LUA_SOURCE_CLASS = { Script = true, LocalScript = true, ModuleScript = true },
		bridgeRole = "edit",
		bridgeRuntimeId = "p1-p5-tracker",
		shouldIgnoreInstance = function() return false end,
		invalidateRuntimeExportCache = function() end,
		loadPendingStudioChanges = function() return nil end,
		savePendingStudioChanges = function() return nil end,
	}, { Workspace = true, ReplicatedStorage = true })
	tracker.observeExports(
		function() return true end,
		function() end,
		function(_, propName: string)
			return propName == "Name" or propName == "Parent" or propName == "Archivable"
		end
	)
	tracker.getState({
		start = true,
		trackingGuardId = "p1-guard",
		services = { "Workspace", "ReplicatedStorage" },
		nativeAttributeRelay = true,
		deferNativeTracking = true,
		lazyTracking = true,
		syncbackProperties = true,
		propertyNamesByClass = {
			Part = {
				"Transparency",
				"Name",
				"Reflectance",
				"Position",
				"Mass",
			},
		},
	})
	assert(relayNotify ~= nil, "native attribute relay ObjectValue must be created")
	for _, cb in ipairs(notifyChangedCbs) do
		cb(relayNotify)
	end
	local lazyState = tracker.getState({
		start = true,
		trackingGuardId = "p1-guard",
		services = { "Workspace", "ReplicatedStorage" },
		lazyTracking = true,
		propertyNamesByClass = {
			Part = {
				"Transparency",
				"Name",
				"Reflectance",
				"Position",
				"Mass",
			},
		},
	})
	assert(lazyState.connectedInstances == 0, `Workspace must be lazily observed (connected={tostring(lazyState.connectedInstances)})`)

	local genBeforeSample = tracker.serviceGeneration("Workspace")
	lazyPart.Transparency = 0.5
	tracker.samplePropertyChange(lazyPart, "Transparency")
	assert(
		tracker.serviceGeneration("Workspace") > genBeforeSample,
		"samplePropertyChange must record dirty change for lazily observed Workspace"
	)

	local genBeforeP1 = tracker.serviceGeneration("Workspace")
	tracker.beginChangeJournal("tx-p1-native", { "ReplicatedStorage" })
	tracker.beginNativeImportObservations("tx-p1-native", nil, { ReplicatedStorage = true })
	lazyPart.Name = "RenamedDuringRepImport"
	for _, cb in ipairs(table.clone(itemChangedListeners)) do
		cb(lazyPart, "Name")
	end
	assert(
		tracker.serviceGeneration("Workspace") > genBeforeP1,
		"P1: ItemChanged in lazily observed Workspace must not be dropped during ReplicatedStorage native import"
	)
	tracker.finishNativeImportObservations("tx-p1-native", {})
	tracker.finishChangeJournal("tx-p1-native")

	local genBeforeMass = tracker.serviceGeneration("Workspace")
	tracker.beginChangeJournal("tx-bulk-vel", { "Workspace" })
	tracker.setBulkExpectedProperties({ CFrame = { [lazyPart] = roblox.CFrame.new(1, 2, 3) } }, { lazyPart, "CFrame", roblox.CFrame.new(1, 2, 3) })
	otherPart.Mass = 42
	for _, cb in ipairs(table.clone(itemChangedListeners)) do
		cb(otherPart, "Mass")
	end
	assert(
		tracker.serviceGeneration("Workspace") > genBeforeMass,
		"Mass must not be ignored by removed BULK_IGNORED_ECHO_PROPS"
	)
	local genBeforeOtherPos = tracker.serviceGeneration("Workspace")
	otherPart.Position = roblox.Vector3.new(9, 8, 7)
	for _, cb in ipairs(table.clone(itemChangedListeners)) do
		cb(otherPart, "Position")
	end
	assert(
		tracker.serviceGeneration("Workspace") > genBeforeOtherPos,
		"Position on another instance must not be ignored during activeBulkWrite on lazyPart.CFrame"
	)
	tracker.setBulkExpectedProperties(nil)
	tracker.finishChangeJournal("tx-bulk-vel")
	tracker.stop()
end

do
	local function makeColorInst(name: string, className: string, parent: any, colorProp: string, isBasePart: boolean)
		local props: { [string]: any } = {
			Name = name,
			ClassName = className,
			Parent = parent,
			[colorProp] = roblox.Color3.fromRGB(100, 100, 100),
		}
		local obj: any
		obj = setmetatable({}, {
			__index = function(_, k)
				if k == 1 then return obj end
				if k == "GetChildren" then return function() return {} end end
				if k == "GetFullName" then return function() return "Workspace." .. props.Name end end
				if k == "IsA" then return function(_, c) return c == className or (isBasePart and c == "BasePart") end end
				if k == "IsDescendantOf" then return function() return true end end
				return props[k]
			end,
			__newindex = function(_, k, v)
				if isBasePart and k == "Color" then
					props.Color = roblox.Color3.new(
						math.floor(v.R * 255 + 1e-4) / 255,
						math.floor(v.G * 255 + 1e-4) / 255,
						math.floor(v.B * 255 + 1e-4) / 255
					)
				else
					props[k] = v
				end
			end,
		})
		return obj
	end
	local fakeChildren = {}
	local fakeWs: any = {
		Name = "Workspace",
		ClassName = "Workspace",
		GetChildren = function() return fakeChildren end,
		FindFirstChild = function(_, name)
			for _, child in ipairs(fakeChildren) do
				if child.Name == name then return child end
			end
			return nil
		end,
		FindFirstChildOfClass = function() return nil end,
		GetFullName = function() return "Workspace" end,
	}
	local part = makeColorInst("StepPart", "Part", fakeWs, "Color", true)
	local decal = makeColorInst("StepDecal", "Decal", fakeWs, "Color3", false)
	local light = makeColorInst("StepLight", "PointLight", fakeWs, "Color", false)
	local label = makeColorInst("StepLabel", "TextLabel", fakeWs, "TextColor3", false)
	fakeChildren[1] = part
	fakeChildren[2] = decal
	fakeChildren[3] = light
	fakeChildren[4] = label

	local api = buildReviewHarness({ workspace = fakeWs })
	api.withRequestLease("l-p2-inst", false, api.beginTransaction, {
		transactionId = "tx-p2-inst",
		services = { "Workspace" },
	})
	local targetColor = { _type = "Color3", r = 101 / 255, g = 100 / 255, b = 100 / 255 }
	local instRes = api.withRequestLease("l-p2-inst", false, api.applyChanges, {
		transactionId = "tx-p2-inst",
		services = { "Workspace" },
		propertyChanges = {
			{ service = "Workspace", pathSegments = { "Workspace", "StepPart" }, pathOrdinals = { 1, 1 }, className = "Part", properties = { Color = targetColor } },
			{ service = "Workspace", pathSegments = { "Workspace", "StepDecal" }, pathOrdinals = { 1, 1 }, className = "Decal", properties = { Color3 = targetColor } },
			{ service = "Workspace", pathSegments = { "Workspace", "StepLight" }, pathOrdinals = { 1, 1 }, className = "PointLight", properties = { Color = targetColor } },
			{ service = "Workspace", pathSegments = { "Workspace", "StepLabel" }, pathOrdinals = { 1, 1 }, className = "TextLabel", properties = { TextColor3 = targetColor } },
		},
	})
	assert(instRes.propertyUpdated == 4 and instRes.noops == 0, `P2: per-instance 100/255->101/255 must update all 4 (got updated={instRes.propertyUpdated}, noops={instRes.noops}, errors={tostring(instRes.errors)}, detail={tostring(instRes.error or (instRes.errorDetails and instRes.errorDetails[1] and (instRes.errorDetails[1].error or instRes.errorDetails[1].message)))})`)
	assert(math.round(part.Color.R * 255) == 101, "P2: per-instance write must move Part.Color to 101/255")
	assert(math.round(decal.Color3.R * 255) == 101, "P2: per-instance write must move Decal.Color3 to 101/255")
	assert(math.round(light.Color.R * 255) == 101, "P2: per-instance write must move PointLight.Color to 101/255")
	assert(math.round(label.TextColor3.R * 255) == 101, "P2: per-instance write must move TextLabel.TextColor3 to 101/255")
	api.withRequestLease("l-p2-inst", false, api.commitTransaction, { transactionId = "tx-p2-inst" })
	part.Color = roblox.Color3.fromRGB(100, 100, 100)
	decal.Color3 = roblox.Color3.fromRGB(100, 100, 100)
	light.Color = roblox.Color3.fromRGB(100, 100, 100)
	label.TextColor3 = roblox.Color3.fromRGB(100, 100, 100)

	local stringsB64 = base64Encode(buffer.fromstring("StepPart\0Part\0StepDecal\0Decal\0StepLight\0PointLight\0StepLabel\0TextLabel"))
	api.withRequestLease("l-p2-bulk", false, api.beginTransaction, {
		transactionId = "tx-p2-bulk",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local bulkRes = api.withRequestLease("l-p2-bulk", false, api.applyBulkProperties, {
		transactionId = "tx-p2-bulk",
		service = "Workspace",
		tree = {
			strings = stringsB64,
			stringCount = 8,
			nodeOffset = 0,
			nodeCount = 4,
			totalNodes = 4,
			nodes = packNodes({ { 0, 1, 0, 1 }, { 0, 1, 2, 3 }, { 0, 1, 4, 5 }, { 0, 1, 6, 7 } }),
		},
		columns = {
			{ propertyName = "Color", encoding = "color3", count = 2, rows = packU32({ 1, 3 }), values = packVec3({ { 101 / 255, 100 / 255, 100 / 255 }, { 101 / 255, 100 / 255, 100 / 255 } }) },
			{ propertyName = "Color3", encoding = "color3", count = 1, rows = packU32({ 2 }), values = packVec3({ { 101 / 255, 100 / 255, 100 / 255 } }) },
			{ propertyName = "TextColor3", encoding = "color3", count = 1, rows = packU32({ 4 }), values = packVec3({ { 101 / 255, 100 / 255, 100 / 255 } }) },
		},
	})
	assert(bulkRes.propertyUpdated == 4, "P2: bulk 100/255->101/255 must update all 4")
	local rb = api.withRequestLease("l-p2-bulk", false, api.rollbackTransaction, { transactionId = "tx-p2-bulk" })
	assert(rb.ok == true and rb.state == "rolledBack")
	assert(math.round(part.Color.R * 255) == 100, "P2: bulk rollback must restore Part.Color to 100/255")
	assert(math.round(decal.Color3.R * 255) == 100, "P2: bulk rollback must restore Decal.Color3 to 100/255")
	assert(math.round(light.Color.R * 255) == 100, "P2: bulk rollback must restore PointLight.Color to 100/255")
	assert(math.round(label.TextColor3.R * 255) == 100, "P2: bulk rollback must restore TextLabel.TextColor3 to 100/255")
end

do
	local attrs1: { [string]: any } = { A = "1:string\0B=2" }
	local attrs2: { [string]: any } = { A = "1", B = "2" }
	local nanPropValue = 0 / 0
	local function makeAttrAndNanPart(name: string, parent: any, attrs: { [string]: any })
		local propVal: any = nanPropValue
		local obj: any
		obj = setmetatable({}, {
			__index = function(_, k)
				if k == 1 then return obj end
				if k == "Name" then return name end
				if k == "ClassName" then return "Part" end
				if k == "Parent" then return parent end
				if k == "GetChildren" then return function() return {} end end
				if k == "GetFullName" then return function() return "Workspace." .. name end end
				if k == "GetAttribute" then return function(_, ak) return attrs[ak] end end
				if k == "GetAttributes" then return function() return table.clone(attrs) end end
				if k == "SetAttribute" then return function(_, ak, av) attrs[ak] = av end end
				if k == "CustomNumber" then return propVal end
				return nil
			end,
			__newindex = function(_, k, v)
				if k == "CustomNumber" then
					propVal = v
				end
			end,
		})
		return obj
	end
	local fakeChildren = {}
	local fakeWs: any = {
		Name = "Workspace",
		ClassName = "Workspace",
		GetChildren = function() return fakeChildren end,
		FindFirstChild = function(_, name)
			for _, child in ipairs(fakeChildren) do
				if child.Name == name then return child end
			end
			return nil
		end,
		FindFirstChildOfClass = function() return nil end,
		GetFullName = function() return "Workspace" end,
	}
	local pA = makeAttrAndNanPart("ColA", fakeWs, attrs1)
	local pB = makeAttrAndNanPart("ColB", fakeWs, attrs2)
	fakeChildren[1] = pA
	fakeChildren[2] = pB

	local api = buildReviewHarness({ workspace = fakeWs })
	api.withRequestLease("l-p6", false, api.beginTransaction, {
		transactionId = "tx-p6",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local stringsB64 = base64Encode(buffer.fromstring("ColA\0Part\0ColB"))
	local res = api.withRequestLease("l-p6", false, api.applyBulkProperties, {
		transactionId = "tx-p6",
		service = "Workspace",
		returnOldValues = true,
		tree = {
			strings = stringsB64,
			stringCount = 3,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			nodes = packNodes({ { 0, 1, 0, 1 }, { 0, 1, 2, 1 } }),
		},
		columns = {
			{ kind = "attributes_replace", propertyName = "@attributes", encoding = "indexed", count = 2, rows = packU32({ 1, 2 }), palette = { { Done = true } }, indices = packU32({ 0, 0 }) },
			{ propertyName = "CustomNumber", encoding = "indexed", count = 2, rows = packU32({ 1, 2 }), palette = { 7 }, indices = packU32({ 0, 0 }) },
		},
	})
	assert(res.ok == true and type(res.oldValueColumns) == "table" and #res.oldValueColumns == 2, "P6 & scalar indexed NaN must succeed")
	local attrCol = res.oldValueColumns[1]
	assert(
		attrCol.kind == "attributes_replace" and #attrCol.palette == 2,
		`P6: distinct attribute maps must produce 2 palette entries (got {#attrCol.palette})`
	)
	local nanCol = res.oldValueColumns[2]
	assert(
		nanCol.propertyName == "CustomNumber" and nanCol.encoding == "indexed" and #nanCol.palette == 1,
		"scalar indexed property with NaN old values must encode via NAN_SENTINEL without throwing"
	)
	api.withRequestLease("l-p6", false, api.commitTransaction, { transactionId = "tx-p6" })
end

do
	local api, workspace = buildHarness()
	local model = roblox.Instance.new("Model")
	model.Name = "EditablePkg"
	model.Parent = workspace
	local link = roblox.Instance.new("PackageLink")
	link.Name = "PackageLink"
	link.Parent = model
	local part = roblox.Instance.new("Part")
	part.Name = "Inside"
	part.Transparency = 0
	part.Parent = model

	api.withRequestLease("lease-pkg-ok", false, api.beginTransaction, {
		transactionId = "tx-bulk-pkg-ok",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local stringsB64 = base64Encode(buffer.fromstring("EditablePkg\0Model\0Inside\0Part"))
	local res = api.withRequestLease("lease-pkg-ok", false, api.applyBulkProperties, {
		transactionId = "tx-bulk-pkg-ok",
		service = "Workspace",
		hasPackageTargets = true,
		tree = {
			strings = stringsB64,
			stringCount = 4,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			nodes = packNodes({ { 0, 1, 0, 1 }, { 1, 1, 2, 3 } }),
		},
		columns = {
			{
				propertyName = "Transparency",
				encoding = "f32_const",
				count = 1,
				rows = packU32({ 2 }),
				value = 0.5,
			},
		},
	})
	assert(res.ok == true and res.propertyUpdated == 1 and res.verified == 1, "bulk patch with hasPackageTargets must edit package descendant")
	assert(math.abs(part.Transparency - 0.5) < 1e-5, "package descendant property not updated")
	local commit = api.withRequestLease("lease-pkg-ok", false, api.commitTransaction, {
		transactionId = "tx-bulk-pkg-ok",
	})
	assert(commit.ok == true and commit.state == "committed", "package bulk commit failed")
end

do
	local props: { [string]: any } = {
		Name = "Solid",
		ClassName = "UnionOperation",
		CollisionFidelity = roblox.Enum.CollisionFidelity.Default,
	}
	local fakeWs: any
	local union: any
	union = setmetatable({}, {
		__index = function(_, k)
			if k == 1 then return union end
			if k == "Parent" then return fakeWs end
			if k == "GetChildren" then return function() return {} end end
			if k == "GetFullName" then return function() return "Workspace.Solid" end end
			if k == "IsA" then return function(_, c) return c == "UnionOperation" or c == "TriangleMeshPart" or c == "BasePart" end end
			if k == "IsDescendantOf" then return function() return true end end
			return props[k]
		end,
		__newindex = function(_, k, v)
			props[k] = v
		end,
	})
	fakeWs = {
		Name = "Workspace",
		ClassName = "Workspace",
		GetChildren = function() return { union } end,
		FindFirstChild = function(_, name) return if name == "Solid" then union else nil end,
		FindFirstChildOfClass = function() return nil end,
		GetFullName = function() return "Workspace" end,
	}
	local api = buildReviewHarness({ workspace = fakeWs })
	api.withRequestLease("lease-solid-cf", false, api.beginTransaction, {
		transactionId = "tx-solid-cf",
		services = { "Workspace" },
	})
	local verifyRes = api.withRequestLease("lease-solid-cf", false, api.applyChanges, {
		transactionId = "tx-solid-cf",
		services = { "Workspace" },
		verifyOnly = true,
		propertyChanges = {
			{
				service = "Workspace",
				pathSegments = { "Workspace", "Solid" },
				pathOrdinals = { 1, 1 },
				className = "UnionOperation",
				properties = {
					CollisionFidelity = { _type = "EnumItem", enumType = "Enum.CollisionFidelity", name = "Box" },
				},
			},
		},
	})
	assert(
		#(verifyRes.verifyMismatches or {}) == 1,
		`P4: UnionOperation Default vs Box must report a verify mismatch (got mismatches={tostring(verifyRes.verifyMismatches and #verifyRes.verifyMismatches)})`
	)
	local applyRes = api.withRequestLease("lease-solid-cf", false, api.applyChanges, {
		transactionId = "tx-solid-cf",
		services = { "Workspace" },
		propertyChanges = {
			{
				service = "Workspace",
				pathSegments = { "Workspace", "Solid" },
				pathOrdinals = { 1, 1 },
				className = "UnionOperation",
				properties = {
					CollisionFidelity = { _type = "EnumItem", enumType = "Enum.CollisionFidelity", name = "Box" },
				},
			},
		},
	})
	assert(applyRes.propertyUpdated == 1 and applyRes.noops == 0, `P4: Default -> Box on UnionOperation must be written, not treated as a no-op (updated={tostring(applyRes.propertyUpdated)} noops={tostring(applyRes.noops)} errors={tostring(applyRes.errors)} detail={tostring(applyRes.error or (applyRes.errorDetails and applyRes.errorDetails[1] and (applyRes.errorDetails[1].error or applyRes.errorDetails[1].message)))})`)
	assert(union.CollisionFidelity == roblox.Enum.CollisionFidelity.Box, "P4: UnionOperation.CollisionFidelity must become Box")
	api.withRequestLease("lease-solid-cf", false, api.commitTransaction, {
		transactionId = "tx-solid-cf",
	})
end

do
	local itemChangedCbs: { (any, any) -> () } = {}
	local notifyChangedCbs: { (any) -> () } = {}
	local relayNotify: any = nil
	local coupledProps: { [string]: any } = { Name = "CoupledPart", ClassName = "Part", Transparency = 0, Reflectance = 0, Archivable = true }
	local attrMap: { [string]: any } = { A = 1, B = 1, C = 1 }
	local fakeWs: any
	local coupledPart: any
	local attrPart: any
	coupledPart = setmetatable({}, {
		__index = function(_, k)
			if k == 1 then return coupledPart end
			if k == "Parent" then return fakeWs end
			if k == "GetChildren" then return function() return {} end end
			if k == "GetDescendants" then return function() return {} end end
			if k == "GetFullName" then return function() return "Workspace.CoupledPart" end end
			if k == "GetDebugId" then return function() return "dbg-coupled" end end
			if k == "GetAttributes" then return function() return {} end end
			if k == "GetAttribute" then return function() return nil end end
			if k == "IsA" then return function(_, c) return c == "Part" or c == "BasePart" end end
			if k == "IsDescendantOf" then return function(_, a) return a == fakeWs or a == fakeWs.Parent end end
			if k == "AttributeChanged" then return { Connect = function() return { Disconnect = function() end } end } end
			return coupledProps[k]
		end,
		__newindex = function(_, k, v)
			coupledProps[k] = v
			for _, cb in ipairs(table.clone(itemChangedCbs)) do
				cb(coupledPart, k)
			end
			if k == "Transparency" then
				coupledProps.Reflectance = 0.77
				for _, cb in ipairs(table.clone(itemChangedCbs)) do
					cb(coupledPart, "Reflectance")
				end
			end
		end,
	})
	attrPart = setmetatable({}, {
		__index = function(_, k)
			if k == 1 then return attrPart end
			if k == "Name" then return "AttrPart" end
			if k == "ClassName" then return "Part" end
			if k == "Archivable" then return true end
			if k == "Parent" then return fakeWs end
			if k == "GetChildren" then return function() return {} end end
			if k == "GetDescendants" then return function() return {} end end
			if k == "GetFullName" then return function() return "Workspace.AttrPart" end end
			if k == "GetDebugId" then return function() return "dbg-attr" end end
			if k == "GetAttributes" then return function() return table.clone(attrMap) end end
			if k == "GetAttribute" then return function(_, ak) return attrMap[ak] end end
			if k == "SetAttribute" then
				return function(_, ak, av)
					attrMap[ak] = av
					if ak == "A" then
						attrMap.B = 99
					end
				end
			end
			if k == "IsA" then return function(_, c) return c == "Part" or c == "BasePart" end end
			if k == "IsDescendantOf" then return function(_, a) return a == fakeWs or a == fakeWs.Parent end end
			if k == "AttributeChanged" then return { Connect = function() return { Disconnect = function() end } end } end
			return nil
		end,
	})
	local wsKids = { coupledPart, attrPart }
	fakeWs = {
		Name = "Workspace",
		ClassName = "Workspace",
		Archivable = true,
		GetChildren = function() return wsKids end,
		GetDescendants = function() return wsKids end,
		FindFirstChildOfClass = function() return nil end,
		GetFullName = function() return "Workspace" end,
		GetDebugId = function() return "dbg-ws" end,
		IsDescendantOf = function(_, a) return a == fakeWs.Parent end,
		IsA = function(_, c) return c == "Workspace" end,
		DescendantAdded = { Connect = function() return { Disconnect = function() end } end },
		DescendantRemoving = { Connect = function() return { Disconnect = function() end } end },
		AttributeChanged = { Connect = function() return { Disconnect = function() end } end },
		Changed = { Connect = function() return { Disconnect = function() end } end },
		GetAttributes = function() return {} end,
	}
	local fakeCoreGui = {
		Name = "CoreGui",
		ClassName = "CoreGui",
		GetChildren = function() return {} end,
	}
	local fakeGame: any = {
		ItemChanged = {
			Connect = function(_, cb)
				itemChangedCbs[#itemChangedCbs + 1] = cb
				return {
					Disconnect = function()
						for idx, fn in ipairs(itemChangedCbs) do
							if fn == cb then
								table.remove(itemChangedCbs, idx)
								break
							end
						end
					end,
				}
			end,
		},
		GetService = function(_, name: string)
			if name == "Workspace" then return fakeWs end
			if name == "CoreGui" then return fakeCoreGui end
			if name == "HttpService" then return { GenerateGUID = function() return "p3p5-guid" end } end
			if name == "RunService" then return { Heartbeat = { Connect = function() return { Disconnect = function() end } end } } end
			if name == "EncodingService" then
				return {
					Base64Encode = function(_, b) return buffer.fromstring(base64Encode(b)) end,
					Base64Decode = function(_, b) return base64Decode(b) end,
				}
			end
			if name == "CollectionService" then
				return {
					GetAllTags = function() return {} end,
					GetTagged = function() return {} end,
					GetTags = function() return {} end,
					TagAdded = { Connect = function() return { Disconnect = function() end } end },
					TagRemoved = { Connect = function() return { Disconnect = function() end } end },
					GetInstanceAddedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
					GetInstanceRemovedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
				}
			end
			if name == "ScriptEditorService" then
				return {
					GetScriptDocuments = function() return {} end,
					FindScriptDocument = function() return nil end,
					TextDocumentDidChange = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidOpen = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidClose = { Connect = function() return { Disconnect = function() end } end },
				}
			end
			if name == "ChangeHistoryService" then
				return {
					TryBeginRecording = function() return nil end,
					FinishRecording = function() end,
				}
			end
			if name == "Selection" then
				return {
					Get = function() return {} end,
					Set = function() end,
				}
			end
			return { GetChildren = function() return {} end, GetDescendants = function() return {} end }
		end,
	}
	fakeWs.Parent = fakeGame
	local p3Env: { [string]: any } = {
		game = fakeGame,
		typeof = function(v: any)
			if v == coupledPart or v == attrPart or v == fakeWs or v == fakeCoreGui then return "Instance" end
			return typeof(v)
		end,
		Enum = roblox.Enum,
		Instance = {
			new = function(className: string)
				if className == "BindableEvent" then
					return {
						Event = { Connect = function() return { Disconnect = function() end } end },
						Fire = function() end,
						Destroy = function() end,
					}
				end
				if className == "ObjectValue" then
					local obj: any = {
						Name = "ObjectValue",
						Archivable = true,
						Changed = {
							Connect = function(_, cb)
								notifyChangedCbs[#notifyChangedCbs + 1] = cb
								return { Disconnect = function() end }
							end,
						},
						SetAttribute = function() end,
						Destroy = function() end,
					}
					relayNotify = obj
					return obj
				end
				return roblox.Instance.new(className)
			end,
		},
		Vector3 = roblox.Vector3,
		Vector2 = roblox.Vector2,
		CFrame = roblox.CFrame,
		Color3 = roblox.Color3,
		UDim = roblox.UDim,
		UDim2 = roblox.UDim2,
		BrickColor = roblox.BrickColor,
		NumberRange = roblox.NumberRange,
		NumberSequence = roblox.NumberSequence,
		NumberSequenceKeypoint = roblox.NumberSequenceKeypoint,
		ColorSequence = roblox.ColorSequence,
		ColorSequenceKeypoint = roblox.ColorSequenceKeypoint,
		Rect = roblox.Rect,
		PhysicalProperties = roblox.PhysicalProperties,
		Font = roblox.Font,
		buffer = buffer,
		bit32 = bit32,
		script = { Parent = setmetatable({}, { __index = function(_, name) return name end }) },
		os = os,
		task = { wait = function() end, delay = function() end, defer = function(fn) fn() end },
	}
	local p3Mods: { [string]: any } = {
		RbxDom = {
			findCanonicalPropertyDescriptor = function(_, propName)
				return { name = propName, scriptability = "ReadWrite", dataType = "Float32" }
			end,
			getContentPropertyAliases = function() return {} end,
			getPropertyNames = function() return { "Transparency", "Reflectance" } end,
			getReferencePropertyNames = function() return {} end,
		},
	}
	local function loadP3(name: string)
		if p3Mods[name] then return p3Mods[name] end
		local src = fs.readFile("tools/plugin_ws_bridge/" .. name .. ".module.luau"):gsub("\r\n", "\n")
		p3Mods[name] = luau.load(src, { environment = p3Env, debugName = name })()
		return p3Mods[name]
	end
	p3Env.require = loadP3
	local tracker = loadP3("BridgeStudioChanges").create({
		LUA_SOURCE_CLASS = { Script = true, LocalScript = true, ModuleScript = true },
		bridgeRole = "edit",
		bridgeRuntimeId = "p3-p5-real-bulk",
		shouldIgnoreInstance = function() return false end,
		invalidateRuntimeExportCache = function() end,
		loadPendingStudioChanges = function() return nil end,
		savePendingStudioChanges = function() return nil end,
	}, { Workspace = true })
	tracker.getState({
		start = true,
		trackingGuardId = "p3p5-guard",
		services = { "Workspace" },
		nativeAttributeRelay = true,
		deferNativeTracking = true,
		lazyTracking = true,
		syncbackProperties = true,
	})
	assert(relayNotify ~= nil, "P3/P5: relay must be created")
	for _, cb in ipairs(notifyChangedCbs) do
		cb(relayNotify)
	end
	tracker.getState({
		start = true,
		trackingGuardId = "p3p5-guard",
		services = { "Workspace" },
		lazyTracking = true,
	})
	local syncApi = loadP3("BridgeEditorSync").create({
		stats = {
			requests = 0, lastMs = 0, lastAtUnix = 0, lastOk = true,
			propertyUpdated = 0, attributeUpdated = 0, noops = 0, errors = 0,
			sourceCreated = 0, sourceUpdated = 0, sourceDeleted = 0,
			instanceCreated = 0, instanceReplaced = 0, instanceDeleted = 0,
		},
		allowedServices = { Workspace = true },
		maxChangesPerRequest = 5000,
		runtimeId = "p3-p5-session",
		assertSessionOwnership = function() end,
		captureSessionOwnership = function() return 1 end,
		currentSessionOwnership = function() return 1 end,
		withSessionOwnership = function(_, fn) return fn() end,
		studioChangeGeneration = function(s) return tracker.serviceGeneration(s) end,
		nativeExportGeneration = function() return 0 end,
		attributeObservation = function() return { active = true, generation = 0 } end,
		beginStudioChangeJournal = tracker.beginChangeJournal,
		drainStudioChangeJournal = tracker.drainChangeJournal,
		retireStudioChangeJournal = tracker.retireChangeJournal,
		expireStudioChangeJournal = tracker.expireChangeJournal,
		finishStudioChangeJournal = tracker.finishChangeJournal,
		finishEditorTransactionExpectation = function() end,
		invalidateService = function() end,
		beginStudioChangeSuppression = function() end,
		endStudioChangeSuppression = function() end,
		expectParentChange = function() end,
		expectPropertyEvent = function() end,
		expectAttributeEvent = tracker.expectAttributeEvent,
		cancelExpectedEvent = tracker.cancelExpectedEvent,
		samplePropertyChange = function() end,
		beginNativeImportObservations = function() end,
		finishNativeImportObservations = function() return {} end,
		updateStatus = function() end,
		setBulkExpectedProperties = tracker.setBulkExpectedProperties,
		setBulkPropertySignalPaused = tracker.setBulkPropertySignalPaused,
		hasBulkSideEffectObserved = tracker.hasBulkSideEffectObserved,
		isItemChangedActive = tracker.isItemChangedActive,
		captureTerrainProof = function() return "terrain-ok" end,
		noteBulkExportChange = tracker.noteBulkExportChange,
		structureGeneration = function() return 1 end,
	})

	local genBeforeP3 = tracker.serviceGeneration("Workspace")
	local stringsP3 = base64Encode(buffer.fromstring("CoupledPart\0Part\0AttrPart"))
	syncApi.withRequestLease("l-p3", false, syncApi.beginTransaction, {
		transactionId = "tx-p3-real",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local resP3 = syncApi.withRequestLease("l-p3", false, syncApi.applyBulkProperties, {
		transactionId = "tx-p3-real",
		service = "Workspace",
		tree = {
			strings = stringsP3,
			stringCount = 3,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			nodes = packNodes({ { 0, 1, 0, 1 }, { 0, 1, 2, 1 } }),
		},
		columns = {
			{ propertyName = "Transparency", encoding = "f32_const", count = 1, rows = packU32({ 1 }), value = 0.5 },
		},
	})
	assert(resP3.ok == true and resP3.propertyUpdated == 1, "P3: real bulk Transparency write must succeed")
	assert(
		tracker.serviceGeneration("Workspace") > genBeforeP3,
		"P3: coupled serialized Reflectance fired inside real applyBulkProperties must NOT be suppressed"
	)
	local okCommitP3, errCommitP3 = pcall(syncApi.withRequestLease, "l-p3", false, syncApi.commitTransaction, { transactionId = "tx-p3-real" })
	assert(not okCommitP3 and string.find(tostring(errCommitP3), "Reflectance", 1, true) ~= nil, `P3: commitTransaction must reject coupled Reflectance conflict (got ok={okCommitP3}, err={tostring(errCommitP3)})`)
	syncApi.withRequestLease("l-p3", false, syncApi.rollbackTransaction, { transactionId = "tx-p3-real" })

	coupledPart.Transparency = 0
	coupledPart.Reflectance = 0
	syncApi.withRequestLease("l-p3-inline", false, syncApi.beginTransaction, {
		transactionId = "tx-p3-inline",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local resInlineP3 = syncApi.withRequestLease("l-p3-inline", false, syncApi.applyBulkProperties, {
		transactionId = "tx-p3-inline",
		service = "Workspace",
		commit = true,
		expectedTerrainDigest = "terrain-ok",
		tree = {
			strings = stringsP3,
			stringCount = 3,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			nodes = packNodes({ { 0, 1, 0, 1 }, { 0, 1, 2, 1 } }),
		},
		columns = {
			{ propertyName = "Transparency", encoding = "f32_const", count = 1, rows = packU32({ 1 }), value = 0.5 },
		},
	})
	assert(
		resInlineP3.ok == true and resInlineP3.sideEffectObserved == true and resInlineP3.exactRetained == false,
		`P3b: commit=true bulk write must flag sideEffectObserved and exactRetained=false (got sideEffect={tostring(resInlineP3.sideEffectObserved)}, exact={tostring(resInlineP3.exactRetained)})`
	)
	local okInlineP3, errInlineP3 = pcall(syncApi.withRequestLease, "l-p3-inline", false, syncApi.commitTransaction, { transactionId = "tx-p3-inline" })
	assert(
		not okInlineP3 and string.find(tostring(errInlineP3), "Reflectance", 1, true) ~= nil,
		`P3b: commitTransaction after commit=true bulk write must reject coupled Reflectance conflict (got ok={okInlineP3}, err={tostring(errInlineP3)})`
	)
	syncApi.withRequestLease("l-p3-inline", false, syncApi.rollbackTransaction, { transactionId = "tx-p3-inline" })

	syncApi.withRequestLease("l-p5", false, syncApi.beginTransaction, {
		transactionId = "tx-p5-real",
		services = { "Workspace" },
		hasBulkPropertyPatches = true,
	})
	local genBeforeP5 = tracker.serviceGeneration("Workspace")
	local resP5 = syncApi.withRequestLease("l-p5", false, syncApi.applyBulkProperties, {
		transactionId = "tx-p5-real",
		service = "Workspace",
		tree = {
			strings = stringsP3,
			stringCount = 3,
			nodeOffset = 0,
			nodeCount = 2,
			totalNodes = 2,
			nodes = packNodes({ { 0, 1, 0, 1 }, { 0, 1, 2, 1 } }),
		},
		columns = {
			{ kind = "attribute", propertyName = "A", encoding = "indexed", count = 1, rows = packU32({ 2 }), palette = { 2 }, indices = packU32({ 0 }) },
			{ kind = "attribute", propertyName = "C", encoding = "indexed", count = 1, rows = packU32({ 2 }), palette = { 3 }, indices = packU32({ 0 }) },
		},
	})
	assert(resP5.ok == true and resP5.propertyUpdated == 2, "P5: real bulk attribute columns A and C must both update")
	for _, cb in ipairs(notifyChangedCbs) do
		cb(attrPart)
	end
	assert(
		tracker.serviceGeneration("Workspace") > genBeforeP5,
		"P5: real applyBulkProperties must not absorb user edit B=99 made between bulk attribute writes A and C"
	)
	local okCommitP5 = pcall(syncApi.withRequestLease, "l-p5", false, syncApi.commitTransaction, { transactionId = "tx-p5-real" })
	assert(not okCommitP5, "P5: commitTransaction must reject concurrent user attribute edit B=99")
	syncApi.withRequestLease("l-p5", false, syncApi.rollbackTransaction, { transactionId = "tx-p5-real" })
	tracker.stop()
end

do
	local function makeTrackerHarness(opts: { onWait: (() -> ())? })
		local itemChangedCbs: { (any, any) -> () } = {}
		local notifyChangedCbs: { (any) -> () } = {}
		local addedCbs: { [string]: { [any]: (any) -> () } } = { Workspace = {}, ReplicatedStorage = {} }
		local removingCbs: { [string]: { [any]: (any) -> () } } = { Workspace = {}, ReplicatedStorage = {} }
		local fakeClock = 100.0
		local relayNotify: any = nil
		local wsDescs: { any } = {}
		local repDescs: { any } = {}
		local fakeWs: any
		local fakeRep: any
		local function makeSignal(tbl: { [any]: (any) -> () })
			return {
				Connect = function(_, cb)
					local tok = {}
					tbl[tok] = cb
					return { Disconnect = function() tbl[tok] = nil end }
				end,
			}
		end
		local function makeFakeInst(name: string, parent: any, dbgId: string, attrs: { [string]: any }?)
			local attrMap = attrs or {}
			local attrCbs: { [any]: (string) -> () } = {}
			local parentRef = parent
			local inst: any
			inst = setmetatable({
				_attrCbs = attrCbs,
				_fireAttr = function(k: string, v: any)
					attrMap[k] = v
					for _, cb in pairs(table.clone(attrCbs)) do
						cb(k)
					end
				end,
			}, {
				__index = function(_, k)
					if k == "Name" then return name end
					if k == "ClassName" then return "Part" end
					if k == "Archivable" then return true end
					if k == "Parent" then return parentRef end
					if k == "GetDebugId" then return function() return dbgId end end
					if k == "GetChildren" then return function() return {} end end
					if k == "GetDescendants" then return function() return {} end end
					if k == "GetFullName" then return function() return (if parentRef then parentRef.Name .. "." else "") .. name end end
					if k == "GetAttributes" then return function() return table.clone(attrMap) end end
					if k == "GetTags" then return function() return {} end end
					if k == "GetAttribute" then return function(_, ak) return attrMap[ak] end end
					if k == "IsA" then return function(_, c) return c == "Part" or c == "BasePart" end end
					if k == "IsDescendantOf" then
						return function(_, ancestor)
							local cur = parentRef
							while cur ~= nil do
								if cur == ancestor then return true end
								cur = cur.Parent
							end
							return false
						end
					end
					if k == "AttributeChanged" then
						return makeSignal(attrCbs)
					end
					return nil
				end,
				__newindex = function(_, k, v)
					if k == "Parent" then
						parentRef = v
					end
				end,
			})
			return inst
		end
		fakeWs = {
			Name = "Workspace",
			ClassName = "Workspace",
			Archivable = true,
			GetChildren = function() return wsDescs end,
			GetDescendants = function() return table.clone(wsDescs) end,
			GetDebugId = function() return "dbg-ws-root" end,
			GetFullName = function() return "Workspace" end,
			GetAttributes = function() return {} end,
			GetTags = function() return {} end,
			IsA = function(_, c) return c == "Workspace" end,
			IsDescendantOf = function(_, a) return a == fakeWs.Parent end,
			DescendantAdded = makeSignal(addedCbs.Workspace),
			DescendantRemoving = makeSignal(removingCbs.Workspace),
			AttributeChanged = { Connect = function() return { Disconnect = function() end } end },
			Changed = { Connect = function() return { Disconnect = function() end } end },
		}
		fakeRep = {
			Name = "ReplicatedStorage",
			ClassName = "ReplicatedStorage",
			Archivable = true,
			GetChildren = function() return repDescs end,
			GetDescendants = function() return table.clone(repDescs) end,
			GetDebugId = function() return "dbg-rep-root" end,
			GetFullName = function() return "ReplicatedStorage" end,
			GetAttributes = function() return {} end,
			GetTags = function() return {} end,
			IsA = function(_, c) return c == "ReplicatedStorage" end,
			IsDescendantOf = function(_, a) return a == fakeRep.Parent end,
			DescendantAdded = makeSignal(addedCbs.ReplicatedStorage),
			DescendantRemoving = makeSignal(removingCbs.ReplicatedStorage),
			AttributeChanged = { Connect = function() return { Disconnect = function() end } end },
			Changed = { Connect = function() return { Disconnect = function() end } end },
		}
		local fakeCoreGui = { Name = "CoreGui", ClassName = "CoreGui", GetChildren = function() return {} end }
		local fakeGame: any = {
			ItemChanged = {
				Connect = function(_, cb)
					itemChangedCbs[#itemChangedCbs + 1] = cb
					return { Disconnect = function() end }
				end,
			},
			GetService = function(_, name: string)
				if name == "Workspace" then return fakeWs end
				if name == "ReplicatedStorage" then return fakeRep end
				if name == "CoreGui" then return fakeCoreGui end
				if name == "HttpService" then return { GenerateGUID = function() return "n-guid-" .. tostring(math.floor(fakeClock * 1000)) end } end
				if name == "RunService" then return { Heartbeat = { Connect = function() return { Disconnect = function() end } end } } end
				if name == "CollectionService" then
					return {
						GetAllTags = function() return {} end,
						GetTagged = function() return {} end,
						GetTags = function() return {} end,
						TagAdded = { Connect = function() return { Disconnect = function() end } end },
						TagRemoved = { Connect = function() return { Disconnect = function() end } end },
						GetInstanceAddedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
						GetInstanceRemovedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
					}
				end
				if name == "ScriptEditorService" then
					return {
						GetScriptDocuments = function() return {} end,
						TextDocumentDidChange = { Connect = function() return { Disconnect = function() end } end },
						TextDocumentDidOpen = { Connect = function() return { Disconnect = function() end } end },
						TextDocumentDidClose = { Connect = function() return { Disconnect = function() end } end },
					}
				end
				return { GetChildren = function() return {} end, GetDescendants = function() return {} end }
			end,
		}
		fakeWs.Parent = fakeGame
		fakeRep.Parent = fakeGame
		local env: { [string]: any } = {
			game = fakeGame,
			typeof = function(v: any)
				if type(v) == "table" and (v == fakeWs or v == fakeRep or v == fakeCoreGui or v.ClassName == "Part") then
					return "Instance"
				end
				return typeof(v)
			end,
			Enum = roblox.Enum,
			Instance = {
				new = function(className: string)
					if className == "BindableEvent" then
						return {
							Event = { Connect = function() return { Disconnect = function() end } end },
							Fire = function() end,
							Destroy = function() end,
						}
					end
					if className == "ObjectValue" then
						local obj: any = {
							Name = "ObjectValue",
							Archivable = true,
							Changed = {
								Connect = function(_, cb)
									notifyChangedCbs[#notifyChangedCbs + 1] = cb
									return { Disconnect = function() end }
								end,
							},
							SetAttribute = function() end,
							Destroy = function() end,
						}
						relayNotify = obj
						return obj
					end
					return roblox.Instance.new(className)
				end,
			},
			Vector3 = roblox.Vector3,
			CFrame = roblox.CFrame,
			Color3 = roblox.Color3,
			script = { Parent = setmetatable({}, { __index = function(_, name) return name end }) },
			os = {
				clock = function()
					fakeClock += opts.clockStep or 0.02
					return fakeClock
				end,
				time = os.time,
			},
			task = {
				wait = function()
					if opts.onWait then
						opts.onWait()
					end
				end,
				delay = function() end,
				defer = function(fn) fn() end,
			},
		}
		local mods: { [string]: any } = {
			RbxDom = {
				findCanonicalPropertyDescriptor = function(_, propName)
					return { name = propName, scriptability = "ReadWrite", dataType = "Float32" }
				end,
				getContentPropertyAliases = function() return {} end,
				getPropertyNames = function() return { "Name", "Transparency" } end,
				getReferencePropertyNames = function() return {} end,
			},
		}
		local function loadMod(name: string)
			if mods[name] then return mods[name] end
			local src = fs.readFile("tools/plugin_ws_bridge/" .. name .. ".module.luau"):gsub("\r\n", "\n")
			mods[name] = luau.load(src, { environment = env, debugName = name })()
			return mods[name]
		end
		env.require = loadMod
		local tracker = loadMod("BridgeStudioChanges").create({
			LUA_SOURCE_CLASS = { Script = true, LocalScript = true, ModuleScript = true },
			bridgeRole = "edit",
			bridgeRuntimeId = "n1-n4-n6-tracker",
			shouldIgnoreInstance = function() return false end,
			exportInclusionUnder = function()
				return function() return true end
			end,
			exportProofGeneration = function() return 1, nil end,
			captureTerrainProof = function() return "terrain-digest" end,
			observeProofDocuments = function() return {} end,
			invalidateRuntimeExportCache = function() end,
			loadPendingStudioChanges = function() return nil end,
			savePendingStudioChanges = function() return nil end,
		}, { Workspace = true, ReplicatedStorage = true })
		return {
			tracker = tracker,
			fakeWs = fakeWs,
			fakeRep = fakeRep,
			wsDescs = wsDescs,
			repDescs = repDescs,
			addedCbs = addedCbs,
			removingCbs = removingCbs,
			notifyChangedCbs = notifyChangedCbs,
			getRelayNotify = function() return relayNotify end,
			makeFakeInst = makeFakeInst,
			fireAdded = function(svc: string, inst: any)
				for _, cb in pairs(table.clone(addedCbs[svc])) do
					cb(inst)
				end
			end,
			fireRemoving = function(svc: string, inst: any)
				for _, cb in pairs(table.clone(removingCbs[svc])) do
					cb(inst)
				end
			end,
		}
	end

	local userInsertedDuringYield: any = nil
	local deletedDuringYield: any = nil
	local yieldFired = false
	local hN1: any
	hN1 = makeTrackerHarness({
		clockStep = 0.5,
		onWait = function()
			if yieldFired then return end
			yieldFired = true
			userInsertedDuringYield = hN1.makeFakeInst("UserInserted", hN1.fakeWs, "dbg-user-unowned")
			hN1.wsDescs[#hN1.wsDescs + 1] = userInsertedDuringYield
			hN1.fireAdded("Workspace", userInsertedDuringYield)
			deletedDuringYield = hN1.wsDescs[2099]
			deletedDuringYield.Parent = nil
			hN1.fireRemoving("Workspace", deletedDuringYield)
		end,
	})
	hN1.tracker.getState({
		start = true,
		trackingGuardId = "n1-guard",
		services = { "Workspace" },
		nativeAttributeRelay = true,
		deferNativeTracking = true,
		lazyTracking = true,
		syncbackProperties = true,
	})
	for _, cb in ipairs(hN1.notifyChangedCbs) do
		cb(hN1.getRelayNotify())
	end
	hN1.tracker.getState({ start = true, trackingGuardId = "n1-guard", services = { "Workspace" }, lazyTracking = true })
	hN1.tracker.beginChangeJournal("tx-n1", { "Workspace" })
	hN1.tracker.beginNativeImportObservations("tx-n1", nil, { Workspace = true })
	local createdById: { [string]: string } = {}
	for i = 1, 2100 do
		local id = "dbg-imp-" .. tostring(i)
		createdById[id] = "Part"
		hN1.wsDescs[i] = hN1.makeFakeInst("Imp" .. tostring(i), hN1.fakeWs, id)
	end
	local genBeforeN1 = hN1.tracker.serviceGeneration("Workspace")
	local additionsN1 = hN1.tracker.finishNativeImportObservations("tx-n1", createdById)
	assert(yieldFired, "N1: scanSuspendedNativeService must have yielded via task.wait()")
	assert(
		additionsN1.Workspace[userInsertedDuringYield] == nil,
		"N1: user instance inserted during scan yield must NOT be classified as import-created"
	)
	assert(
		hN1.tracker.serviceGeneration("Workspace") > genBeforeN1,
		"N1: user insert/delete during scan yield must mark Workspace dirty"
	)
	assert(
		hN1.tracker.exportDebugIds("Workspace") == nil,
		"N1: stale export metadata cache must NOT be stored after yield-time insert/delete"
	)
	hN1.tracker.finishChangeJournal("tx-n1")
	hN1.tracker.stop()

	local hAdd = makeTrackerHarness({})
	hAdd.tracker.getState({
		start = true,
		trackingGuardId = "add-guard",
		services = { "Workspace" },
		nativeAttributeRelay = true,
		deferNativeTracking = true,
		lazyTracking = true,
		syncbackProperties = true,
	})
	for _, cb in ipairs(hAdd.notifyChangedCbs) do
		cb(hAdd.getRelayNotify())
	end
	hAdd.tracker.getState({ start = true, trackingGuardId = "add-guard", services = { "Workspace" }, lazyTracking = true })
	hAdd.tracker.beginChangeJournal("tx-add", { "Workspace" })
	hAdd.tracker.beginNativeImportObservations("tx-add", nil, {})
	assert(
		next(hAdd.addedCbs.Workspace) ~= nil,
		"SC:2118: additive import must keep DescendantAdded connected regardless of existing count"
	)
	hAdd.tracker.finishNativeImportObservations("tx-add", {})
	hAdd.tracker.finishChangeJournal("tx-add")
	hAdd.tracker.stop()

	local hN4 = makeTrackerHarness({})
	local n4Part = hN4.makeFakeInst("LazyAttr", hN4.fakeWs, "dbg-n4", { Score = 1 })
	hN4.wsDescs[1] = n4Part
	hN4.tracker.getState({
		start = true,
		trackingGuardId = "n4-guard",
		services = { "Workspace" },
		nativeAttributeRelay = true,
		deferNativeTracking = true,
		lazyTracking = true,
		syncbackProperties = true,
	})
	for _, cb in ipairs(hN4.notifyChangedCbs) do
		cb(hN4.getRelayNotify())
	end
	hN4.tracker.getState({ start = true, trackingGuardId = "n4-guard", services = { "Workspace" }, lazyTracking = true })
	local tokN4 = hN4.tracker.expectAttributeEvent(n4Part, "Score", 2)
	hN4.tracker.cancelExpectedEvent(tokN4)
	hN4.tracker.beginChangeJournal("tx-n4", { "Workspace" })
	hN4.tracker.beginNativeImportObservations("tx-n4", nil, {})
	hN4.tracker.finishNativeImportObservations("tx-n4", {})
	hN4.tracker.finishChangeJournal("tx-n4")
	hN4.tracker.stop()
	local activeAttrListeners = 0
	for _ in pairs(n4Part._attrCbs) do
		activeAttrListeners += 1
	end
	assert(
		activeAttrListeners == 0,
		`N4: stopping tracking after lazy promote + native import must leave 0 attribute listeners (got {activeAttrListeners})`
	)

	local hN6 = makeTrackerHarness({})
	local repPart = hN6.makeFakeInst("RepAttr", hN6.fakeRep, "dbg-n6-rep", { Flag = false })
	hN6.repDescs[1] = repPart
	hN6.tracker.getState({
		start = true,
		trackingGuardId = "n6-guard-1",
		services = { "Workspace", "ReplicatedStorage" },
		nativeAttributeRelay = true,
		deferNativeTracking = true,
		lazyTracking = true,
		syncbackProperties = true,
	})
	for _, cb in ipairs(hN6.notifyChangedCbs) do
		cb(hN6.getRelayNotify())
	end
	hN6.tracker.getState({
		start = true,
		trackingGuardId = "n6-guard-1",
		services = { "Workspace", "ReplicatedStorage" },
		lazyTracking = true,
		syncbackProperties = true,
	})
	local proofRes = hN6.tracker.getState({ start = false, capturePushProof = true })
	assert(proofRes.verifiedPushProof ~= nil, "N6: capturePushProof must succeed in test harness")
	hN6.tracker.getState({
		start = true,
		trackingGuardId = "n6-guard-2",
		services = { "Workspace", "ReplicatedStorage" },
		lazyTracking = true,
	})
	local retainRes = hN6.tracker.getState({
		start = false,
		releaseTrackingGuardId = "n6-guard-1",
		retainPushProof = proofRes.verifiedPushProof,
		retainSeconds = 60,
	})
	assert(retainRes.retainedPushProof == proofRes.verifiedPushProof, "N6: retainPushProof must cache relay")
	hN6.tracker.getState({
		start = true,
		trackingGuardId = "n6-guard-3",
		services = { "Workspace" },
		nativeAttributeRelay = true,
		lazyTracking = true,
	})
	local genBeforeN6 = hN6.tracker.serviceGeneration("ReplicatedStorage")
	repPart._fireAttr("Flag", true)
	assert(
		hN6.tracker.serviceGeneration("ReplicatedStorage") > genBeforeN6,
		"N6: lazy ReplicatedStorage outside new request must still observe attribute edits after cached relay destruction"
	)
	hN6.tracker.stop()
end

do
	local esSrc = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.luau"):gsub("\r\n", "\n")
	local fnStart = assert(esSrc:find("\tlocal function finishNativeServiceImport(", 1, true))
	local fnEnd = assert(esSrc:find("\tfunction api.finishBinaryImport(", fnStart, true)) - 1
	local fnChunk = esSrc:sub(fnStart, fnEnd) .. "\nreturn finishNativeServiceImport"

	local function runFinishN3(mode: string)
		local ws = roblox.Instance.new("Folder")
		ws.Name = "Workspace"
		local verifyCalls = 0
		local fakeApi = {
			applyChanges = function(req: any)
				if req.verifyOnly then
					verifyCalls += 1
					return { ok = true, errors = 0, verified = true }
				end
				if mode == "root" then
					return { ok = true, errors = 0, nativeRootWrites = { { index = 1 } } }
				elseif mode == "geometry" then
					return { ok = true, errors = 0, nativeGeometryWrites = { { index = 1 } } }
				end
				return { ok = true, errors = 0 }
			end,
			getLiveSourceBatch = function() return {} end,
		}
		local finishNative = luau.load(fnChunk, {
			environment = {
				BridgeConnection = require("../BridgeConnection.module"),
				BridgeParallel = {
					newWorkBudget = function() return function() return false end end,
					newRequestBudget = function() return function() return false end end,
				},
				ctx = {
					finishNativeImportObservations = function() return {} end,
					invalidateService = function() end,
				},
				pathCacheKey = function(s, o) return table.concat(s, "/") .. table.concat(o, ",") end,
				prepareImportedIdentities = function() return {} end,
				rememberImportedIdentities = function() end,
				beginHistoryRecording = function() return "rec" end,
				runWithStudioChangeSuppression = function(_, cb) cb() end,
				ReferenceOverlay = { apply = function() end },
				describeNativeRootTargets = function() return {} end,
				api = fakeApi,
				game = { GetService = function() return ws end },
			},
			debugName = "finishNativeServiceImport-N3",
		})()
		local session = {
			nativeHeld = {
				readerArmed = true,
				invocationAuthorized = true,
				receivedCreated = 0,
				expectedCreated = 0,
				createdById = {},
				boundPaths = {},
				started = os.clock(),
			},
			totalBytes = 64,
		}
		local transaction = {
			transactionId = "tx-n3-" .. mode,
			assertOwnership = function() end,
			nativeUndo = { prepared = {}, replacements = {} },
		}
		local fin = finishNative(session, transaction, {
			nativePhase = "complete",
			nativeStatus = 4,
			nativeState = 14,
			nativeCreated = 0,
			postImportPropertyChanges = {
				{ service = "Workspace", pathSegments = { "Workspace", "RootPart" }, pathOrdinals = { 1, 1 }, className = "Part", properties = { Transparency = 0.5 } },
			},
			verifyPropertyChanges = {
				{ service = "Workspace", pathSegments = { "Workspace", "RootPart" }, pathOrdinals = { 1, 1 }, className = "Part", properties = { Transparency = 0.5 } },
			},
		})
		return fin, verifyCalls
	end

	local finRoot, callsRoot = runFinishN3("root")
	assert(
		finRoot.postImportVerifySummary == nil and callsRoot == 0,
		`N3: finishNativeServiceImport must skip inline postImportVerifySummary when nativeRootWrites is non-empty (calls={callsRoot})`
	)
	local finGeom, callsGeom = runFinishN3("geometry")
	assert(
		finGeom.postImportVerifySummary == nil and callsGeom == 0,
		`N3: finishNativeServiceImport must skip inline postImportVerifySummary when nativeGeometryWrites is non-empty (calls={callsGeom})`
	)
	local finClean, callsClean = runFinishN3("clean")
	assert(
		finClean.postImportVerifySummary ~= nil and callsClean == 1,
		`N3: finishNativeServiceImport must run inline postImportVerifySummary when neither nativeRootWrites nor nativeGeometryWrites is present (calls={callsClean})`
	)
end

do
	local function makeRigNode(name: string, className: string, parent: any, dbgId: string, children: { any }?)
		local childList = children or {}
		local parentRef = parent
		local node: any
		node = setmetatable({}, {
			__index = function(_, k)
				if k == "Name" then return name end
				if k == "ClassName" then return className end
				if k == "Archivable" then return true end
				if k == "Parent" then return parentRef end
				if k == "GetDebugId" then return function() return dbgId end end
				if k == "GetChildren" then return function() return childList end end
				if k == "GetDescendants" then return function() return childList end end
				if k == "GetFullName" then return function() return (if parentRef then parentRef.Name .. "." else "") .. name end end
				if k == "GetAttributes" then return function() return {} end end
				if k == "GetTags" then return function() return {} end end
				if k == "FindFirstChildOfClass" then
					return function(_, targetClass)
						for _, ch in ipairs(childList) do
							if ch.ClassName == targetClass then return ch end
						end
						return nil
					end
				end
				if k == "IsA" then
					return function(_, c)
						if c == className then return true end
						if className == "Part" and c == "BasePart" then return true end
						if className == "Accessory" and c == "Accoutrement" then return true end
						return false
					end
				end
				if k == "IsDescendantOf" then
					return function(_, anc)
						local cur = parentRef
						while cur ~= nil do
							if cur == anc then return true end
							cur = cur.Parent
						end
						return false
					end
				end
				if k == "AttributeChanged" or k == "Changed" then
					return { Connect = function() return { Disconnect = function() end } end }
				end
				return nil
			end,
			__newindex = function(_, k, v)
				if k == "Parent" then parentRef = v end
			end,
		})
		if parent ~= nil and type(parent) == "table" and type(parent._children) == "table" then
			parent._children[#parent._children + 1] = node
		end
		return node
	end

	local addedCbs: { [any]: (any) -> () } = {}
	local wsDescs: { any } = {}
	local fakeWs: any
	local fakeGame: any
	fakeWs = {
		Name = "Workspace",
		ClassName = "Workspace",
		Archivable = true,
		GetChildren = function() return wsDescs end,
		GetDescendants = function() return table.clone(wsDescs) end,
		GetDebugId = function() return "dbg-rig-ws" end,
		GetFullName = function() return "Workspace" end,
		GetAttributes = function() return {} end,
		GetTags = function() return {} end,
		IsA = function(_, c) return c == "Workspace" end,
		IsDescendantOf = function(_, a) return a == fakeGame end,
		DescendantAdded = {
			Connect = function(_, cb)
				local tok = {}
				addedCbs[tok] = cb
				return { Disconnect = function() addedCbs[tok] = nil end }
			end,
		},
		DescendantRemoving = { Connect = function() return { Disconnect = function() end } end },
		AttributeChanged = { Connect = function() return { Disconnect = function() end } end },
		Changed = { Connect = function() return { Disconnect = function() end } end },
	}
	fakeGame = {
		ItemChanged = { Connect = function() return { Disconnect = function() end } end },
		GetService = function(_, name: string)
			if name == "Workspace" then return fakeWs end
			if name == "CoreGui" then return { Name = "CoreGui", ClassName = "CoreGui", GetChildren = function() return {} end } end
			if name == "HttpService" then return { GenerateGUID = function() return "rig-guid" end } end
			if name == "RunService" then return { Heartbeat = { Connect = function() return { Disconnect = function() end } end } } end
			if name == "CollectionService" then
				return {
					GetAllTags = function() return {} end,
					GetTagged = function() return {} end,
					GetTags = function() return {} end,
					TagAdded = { Connect = function() return { Disconnect = function() end } end },
					TagRemoved = { Connect = function() return { Disconnect = function() end } end },
					GetInstanceAddedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
					GetInstanceRemovedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
				}
			end
			return { GetChildren = function() return {} end, GetDescendants = function() return {} end }
		end,
	}
	fakeWs.Parent = fakeGame
	local rigEnv: { [string]: any } = {
		game = fakeGame,
		typeof = function(v: any)
			if type(v) == "table" and v.ClassName ~= nil then return "Instance" end
			return typeof(v)
		end,
		Enum = roblox.Enum,
		Instance = {
			new = function(cls: string)
				if cls == "BindableEvent" then
					return {
						Event = { Connect = function() return { Disconnect = function() end } end },
						Fire = function() end,
						Destroy = function() end,
					}
				end
				return roblox.Instance.new(cls)
			end,
		},
		Vector3 = roblox.Vector3,
		CFrame = roblox.CFrame,
		Color3 = roblox.Color3,
		script = { Parent = setmetatable({}, { __index = function(_, n) return n end }) },
		os = os,
		task = { wait = function() end, delay = function() end, defer = function(fn) fn() end },
	}
	local rigMods: { [string]: any } = {
		RbxDom = {
			findCanonicalPropertyDescriptor = function(_, p) return { name = p, scriptability = "ReadWrite", dataType = "Float32" } end,
			getContentPropertyAliases = function() return {} end,
			getPropertyNames = function() return { "Name" } end,
			getReferencePropertyNames = function() return {} end,
		},
	}
	local function loadRigMod(n: string)
		if rigMods[n] then return rigMods[n] end
		local src = fs.readFile("tools/plugin_ws_bridge/" .. n .. ".module.luau"):gsub("\r\n", "\n")
		rigMods[n] = luau.load(src, { environment = rigEnv, debugName = n })()
		return rigMods[n]
	end
	rigEnv.require = loadRigMod
	local rigTracker = loadRigMod("BridgeStudioChanges").create({
		LUA_SOURCE_CLASS = { Script = true, LocalScript = true, ModuleScript = true },
		bridgeRole = "edit",
		bridgeRuntimeId = "rig-accessory-test",
		shouldIgnoreInstance = function() return false end,
		exportInclusionUnder = function() return function() return true end end,
		invalidateRuntimeExportCache = function() end,
		loadPendingStudioChanges = function() return nil end,
		savePendingStudioChanges = function() return nil end,
	}, { Workspace = true })
	rigTracker.getState({ start = true, trackingGuardId = "rig-g", services = { "Workspace" }, syncbackProperties = true })
	rigTracker.beginChangeJournal("tx-rig", { "Workspace" })
	rigTracker.beginNativeImportObservations("tx-rig", nil, { Workspace = true })
	local rigChildren = {}
	local rigModel = makeRigNode("Rig", "Model", fakeWs, "dbg-rig-model", rigChildren)
	local hum = makeRigNode("Humanoid", "Humanoid", rigModel, "dbg-rig-hum", {})
	local accChildren = {}
	local acc = makeRigNode("Hat", "Accessory", rigModel, "dbg-rig-acc", accChildren)
	local handleChildren = {}
	local handle = makeRigNode("Handle", "Part", acc, "dbg-rig-handle", handleChildren)
	local engineWeld = makeRigNode("AccessoryWeld", "Weld", handle, "dbg-engine-weld-unowned", {})
	table.insert(rigChildren, hum)
	table.insert(rigChildren, acc)
	table.insert(accChildren, handle)
	table.insert(handleChildren, engineWeld)
	wsDescs = { rigModel, hum, acc, handle, engineWeld }
	local createdById = {
		["dbg-rig-model"] = "Model",
		["dbg-rig-hum"] = "Humanoid",
		["dbg-rig-acc"] = "Accessory",
		["dbg-rig-handle"] = "Part",
	}
	local genBeforeRig = rigTracker.serviceGeneration("Workspace")
	local rigAdditions = rigTracker.finishNativeImportObservations(
		"tx-rig",
		createdById,
		nil,
		{ ["dbg-engine-weld-unowned"] = true, ["dbg-engine-weld-deferred"] = true }
	)
	assert(
		rigTracker.serviceGeneration("Workspace") == genBeforeRig,
		"Rig Part 1: lock-time engine-created instance inside imported tree must not dirty Workspace"
	)
	assert(
		rigAdditions.Workspace[engineWeld] == nil,
		"Rig Part 1: lock-time engine-created instance must not be misclassified as a receipted payload node"
	)
	local deferredWeld = makeRigNode("AnyEngineJoint", "Motor6D", handle, "dbg-engine-weld-deferred", {})
	for _, cb in pairs(table.clone(addedCbs)) do
		cb(deferredWeld)
	end
	assert(
		rigTracker.serviceGeneration("Workspace") == genBeforeRig,
		"Rig Part 1: deferred-frame signal for lock-time engine-created instance must not dirty Workspace"
	)
	local userPart = makeRigNode("AccessoryWeld", "Weld", handle, "dbg-user-weld", {})
	for _, cb in pairs(table.clone(addedCbs)) do
		cb(userPart)
	end
	assert(
		rigTracker.serviceGeneration("Workspace") > genBeforeRig,
		"Rig Part 1: user-inserted Weld not in lock-time engineCreatedById must still dirty Workspace"
	)
	rigTracker.finishChangeJournal("tx-rig")

	local removingCbs: { [number]: (any) -> () } = {}
	local nextRemId = 0
	fakeWs.DescendantRemoving = {
		Connect = function(_, fn)
			nextRemId += 1
			local id = nextRemId
			removingCbs[id] = fn
			return { Disconnect = function() removingCbs[id] = nil end }
		end,
	}
	rigTracker.stop()
	rigTracker = loadRigMod("BridgeStudioChanges").create({
		LUA_SOURCE_CLASS = { Script = true, LocalScript = true, ModuleScript = true },
		bridgeRole = "edit",
		bridgeRuntimeId = "rig-rem-test",
		shouldIgnoreInstance = function() return false end,
		exportInclusionUnder = function() return function() return true end end,
		invalidateRuntimeExportCache = function() end,
		loadPendingStudioChanges = function() return nil end,
		savePendingStudioChanges = function() return nil end,
	}, { Workspace = true })
	rigTracker.getState({ start = true, trackingGuardId = "rig-g2", services = { "Workspace" }, syncbackProperties = true })
	local savedWeld = makeRigNode("HeadWeld", "Weld", handle, "dbg-saved-headweld", {})
	local genRemOk = rigTracker.serviceGeneration("Workspace")
	rigTracker.beginChangeJournal("tx-rig-rem-ok", { "Workspace" })
	rigTracker.beginNativeImportObservations("tx-rig-rem-ok", nil, { Workspace = true })
	for _, cb in pairs(table.clone(removingCbs)) do
		cb(savedWeld)
	end
	savedWeld.Parent = nil
	rigTracker.finishNativeImportObservations(
		"tx-rig-rem-ok",
		{
			["dbg-rig-model"] = "Model",
			["dbg-rig-acc"] = "Accessory",
			["dbg-rig-handle"] = "Part",
			["dbg-saved-headweld"] = "Weld",
		},
		{ ["dbg-saved-headweld"] = true },
		nil
	)
	assert(
		rigTracker.serviceGeneration("Workspace") == genRemOk,
		"Rig Part 1b: lock-time engine removal of saved HeadWeld must not dirty Workspace"
	)
	rigTracker.finishChangeJournal("tx-rig-rem-ok")

	local unownedWeld = makeRigNode("AccessoryWeld", "Weld", handle, "dbg-unowned-weld", {})
	local genRemBad = rigTracker.serviceGeneration("Workspace")
	rigTracker.beginChangeJournal("tx-rig-rem-bad", { "Workspace" })
	rigTracker.beginNativeImportObservations("tx-rig-rem-bad", nil, { Workspace = true })
	for _, cb in pairs(table.clone(removingCbs)) do
		cb(unownedWeld)
	end
	rigTracker.finishNativeImportObservations("tx-rig-rem-bad", {}, nil, nil)
	assert(
		rigTracker.serviceGeneration("Workspace") > genRemBad,
		"Rig Part 1b: removal not in lock-time engineRemovedById must fail closed and dirty Workspace"
	)
	rigTracker.finishChangeJournal("tx-rig-rem-bad")
	rigTracker.stop()

	local recoveryHarness = require("./transaction-recovery-harness")
	local hRig = recoveryHarness.harness(nil, false)
	local ws = hRig.game:GetService("Workspace")
	local incModel = recoveryHarness.outsideNode(hRig, "ImportedRig", ws)
	local incHandle = recoveryHarness.outsideNode(hRig, "Handle", incModel)
	local incWeld = recoveryHarness.outsideNode(hRig, "AccessoryWeld", incHandle)
	incWeld.GetDebugId = function() return "dbg-inc-weld" end
	local sRig = hRig.begin("tx-rig-rollback")
	sRig.mutated = true
	sRig.snapshot.rootCount = 0
	sRig.snapshot.instanceCount = 0
	sRig.nativeUndo = {
		prepared = {
			{
				serviceName = "Workspace",
				target = ws,
				incoming = { incModel },
				outgoing = {},
				retainedLiveRoots = {},
			},
		},
		replacements = {},
		explicitRollback = true,
	}
	sRig.nativeEngineCreatedById = {
		["dbg-inc-weld"] = true,
	}
	sRig.changeJournal = {
		{
			service = "Workspace",
			instance = incWeld,
			currentInstance = incWeld,
			parent = incHandle,
			parentPathSegments = { "Workspace", "ImportedRig", "Handle" },
			structural = true,
			properties = {},
			attributes = {},
		},
	}
	local rbRes = hRig.api.withRequestLease("owner", false, hRig.api.rollbackTransaction, { transactionId = "tx-rig-rollback" })
	assert(rbRes.state == "rolledBack", "Rig Part 2: rollback with proven engine-created child under incoming parent must succeed")
	assert(sRig.rollbackFailed == nil, "Rig Part 2: rollbackFailed must remain nil")
	assert(hRig.r.sessions["tx-rig-rollback"] == nil, "Rig Part 2: rolled-back transaction must be retired")
	assert(incModel.Parent == nil and incWeld.Parent == nil and incWeld.destroyed == true,
		"Rig Part 2: incoming model and its proven engine-created child must both be removed on rollback")

	local incModel2 = recoveryHarness.outsideNode(hRig, "ImportedRig2", ws)
	local incHandle2 = recoveryHarness.outsideNode(hRig, "Handle2", incModel2)
	local userChild = recoveryHarness.outsideNode(hRig, "UserScript", incHandle2)
	userChild.GetDebugId = function() return "dbg-user-child" end
	local sUser = hRig.begin("tx-rig-user-child")
	sUser.mutated = true
	sUser.snapshot.rootCount = 0
	sUser.snapshot.instanceCount = 0
	sUser.nativeUndo = {
		prepared = {
			{
				serviceName = "Workspace",
				target = ws,
				incoming = { incModel2 },
				outgoing = {},
				retainedLiveRoots = {},
			},
		},
		replacements = {},
		explicitRollback = true,
	}
	sUser.changeJournal = {
		{
			service = "Workspace",
			instance = userChild,
			currentInstance = userChild,
			parent = incHandle2,
			parentPathSegments = { "Workspace", "ImportedRig2", "Handle2" },
			structural = true,
			properties = {},
			attributes = {},
		},
	}
	local okUserRb, errUserRb = pcall(hRig.api.withRequestLease, "owner", false, hRig.api.rollbackTransaction, { transactionId = "tx-rig-user-child" })
	assert(
		not okUserRb and tostring(errUserRb):find("Could not preserve concurrent Studio edit", 1, true) ~= nil,
		`D1: rollback must refuse to destroy user-created child under incoming parent (got ok={tostring(okUserRb)}, err={tostring(errUserRb)})`
	)
	assert(userChild.destroyed ~= true, "D1: user-created child must be preserved, not destroyed")
	hRig.r.sessions["tx-rig-user-child"] = nil

	local sFailed = hRig.begin("tx-failed-export-block")
	sFailed.rollbackFailed = "injected failed rollback"
	local okExp, expErr = pcall(function()
		return hRig.api.withRequestLease("pull-lease", false, hRig.api.beginBinaryExport, { exportId = "exp-blocked" })
	end)
	assert(
		not okExp and tostring(expErr):find("failed rollback retained for recovery", 1, true) ~= nil,
		`Rig Part 3: beginBinaryExport must refuse while rollbackFailed is retained (got ok={tostring(okExp)}, err={tostring(expErr)})`
	)
end

do
	local itemChangedCbs: { [number]: (any, any) -> () } = {}
	local nextIcId = 0
	local function makeSignal()
		local cbs = {}
		local id = 0
		return {
			Connect = function(_, fn)
				id += 1
				local myId = id
				cbs[myId] = fn
				return { Disconnect = function() cbs[myId] = nil end }
			end,
		}, cbs
	end
	local wsChangedSig = makeSignal()
	local wsAddedSig = makeSignal()
	local wsRemovingSig = makeSignal()
	local children = {}
	local fakeWs: any
	fakeWs = {
		Name = "Workspace",
		ClassName = "Workspace",
		Parent = nil,
		Changed = wsChangedSig,
		DescendantAdded = wsAddedSig,
		DescendantRemoving = wsRemovingSig,
		GetChildren = function() return children end,
		GetDescendants = function() return children end,
		IsA = function(_, c: string) return c == "Workspace" or c == "Instance" end,
		IsDescendantOf = function(_, a: any) return a == fakeWs end,
		GetAttributes = function() return {} end,
		GetAttributeChangedSignal = function() return makeSignal() end,
		AttributeChanged = makeSignal(),
		GetDebugId = function() return "dbg-ws-r6" end,
	}
	local fakeGame: any
	fakeGame = {
		ItemChanged = {
			Connect = function(_, fn)
				nextIcId += 1
				local myId = nextIcId
				itemChangedCbs[myId] = fn
				return { Disconnect = function() itemChangedCbs[myId] = nil end }
			end,
		},
		GetService = function(_, name: string)
			if name == "Workspace" then return fakeWs end
			if name == "RunService" then return { Heartbeat = makeSignal() } end
			if name == "CollectionService" then
				return {
					GetAllTags = function() return {} end,
					GetTags = function() return {} end,
					GetTagged = function() return {} end,
					TagAdded = makeSignal(),
					TagRemoved = makeSignal(),
					GetInstanceAddedSignal = function() return makeSignal() end,
					GetInstanceRemovedSignal = function() return makeSignal() end,
				}
			end
			if name == "ScriptEditorService" then
				return {
					GetScriptDocuments = function() return {} end,
					FindScriptDocument = function() return nil end,
					TextDocumentDidChange = makeSignal(),
					TextDocumentDidOpen = makeSignal(),
					TextDocumentDidClose = makeSignal(),
				}
			end
			return { GetChildren = function() return {} end, GetDescendants = function() return {} end }
		end,
	}
	fakeWs.Parent = fakeGame
	local function makeObj(name: string, className: string, isBasePart: boolean)
		local obj: any = {
			Name = name,
			ClassName = className,
			Parent = fakeWs,
			GetChildren = function() return {} end,
			GetDescendants = function() return {} end,
			IsA = function(_, c: string)
				return c == className or c == "Instance" or (isBasePart and c == "BasePart")
			end,
			IsDescendantOf = function(_, a: any) return a == fakeWs or a == fakeGame end,
			GetAttributes = function() return {} end,
			AttributeChanged = makeSignal(),
			Changed = makeSignal(),
			GetDebugId = function() return "dbg-" .. name end,
		}
		children[#children + 1] = obj
		return obj
	end
	local constrained = makeObj("Constrained", "DoubleConstrainedValue", false)
	constrained.Value = 5
	constrained.MaxValue = 5
	local partA = makeObj("PartA", "Part", true)
	local partB = makeObj("PartB", "Part", true)
	partB.CFrame = roblox.CFrame.new(1, 2, 3)

	local r6Env: { [string]: any } = {
		game = fakeGame,
		typeof = function(v: any)
			if v == fakeWs or v == constrained or v == partA or v == partB then
				return "Instance"
			end
			return typeof(v)
		end,
		Enum = roblox.Enum,
		Instance = {
			new = function(className: string)
				if className == "BindableEvent" then
					return { Event = makeSignal(), Fire = function() end, Destroy = function() end }
				end
				return roblox.Instance.new(className)
			end,
		},
		Vector3 = roblox.Vector3,
		CFrame = roblox.CFrame,
		Color3 = roblox.Color3,
		script = { Parent = setmetatable({}, { __index = function(_, name) return name end }) },
		os = os,
		task = { delay = function() end, defer = function(fn) fn() end },
	}
	local r6Mods: { [string]: any } = {
		RbxDom = {
			findCanonicalPropertyDescriptor = function(_, propName)
				return { name = propName, scriptability = "ReadWrite", dataType = "Float32" }
			end,
			getContentPropertyAliases = function() return {} end,
			getPropertyNames = function(_, cls: string)
				if cls == "DoubleConstrainedValue" then
					return { "Name", "Parent", "MinValue", "MaxValue", "Value" }
				end
				return { "Name", "Parent", "Transparency", "Color", "CFrame" }
			end,
		},
	}
	local function loadR6(name: string)
		if r6Mods[name] then return r6Mods[name] end
		local src = fs.readFile("tools/plugin_ws_bridge/" .. name .. ".module.luau"):gsub("\r\n", "\n")
		r6Mods[name] = luau.load(src, { environment = r6Env, debugName = name })()
		return r6Mods[name]
	end
	r6Env.require = loadR6
	local tracker = loadR6("BridgeStudioChanges").create({
		LUA_SOURCE_CLASS = { Script = true, LocalScript = true, ModuleScript = true },
		bridgeRole = "edit",
		bridgeRuntimeId = "r6-side-effect",
		shouldIgnoreInstance = function() return false end,
		invalidateRuntimeExportCache = function() end,
		loadPendingStudioChanges = function() return nil end,
		savePendingStudioChanges = function() return nil end,
	}, { Workspace = true })
	tracker.getState({ start = true, trackingGuardId = "r6-g", services = { "Workspace" }, syncbackProperties = true })
	assert(tracker.isItemChangedActive() == true, "R6: ItemChanged must be active")

	local function fireItemChanged(inst: any, prop: string)
		for _, cb in pairs(table.clone(itemChangedCbs)) do
			cb(inst, prop)
		end
	end

	local expected1 = { Color = { [partA] = roblox.Color3.new(1, 0, 0) } }
	local active1: { any } = { partA, "Color", roblox.Color3.new(1, 0, 0), nil }
	tracker.setBulkExpectedProperties(expected1, active1)
	fireItemChanged(partA, "Color")
	fireItemChanged(partA, "BrickColor")
	fireItemChanged(partA, "Color3uint8")
	assert(
		tracker.hasBulkSideEffectObserved() == false,
		"R6: pure BasePart.Color derived aliases must not flag bulkSideEffectObserved"
	)

	local genBefore = tracker.serviceGeneration("Workspace")
	local expected2 = { MaxValue = { [constrained] = 5 } }
	local active2: { any } = { constrained, "MaxValue", 5, nil }
	tracker.setBulkExpectedProperties(expected2, active2)
	fireItemChanged(constrained, "MaxValue")
	assert(tracker.hasBulkSideEffectObserved() == false, "R6: self property must not flag side effect")
	fireItemChanged(constrained, "Value")
	assert(
		tracker.hasBulkSideEffectObserved() == true,
		"R6: coupled MaxValue -> Value ItemChanged must set bulkSideEffectObserved=true"
	)
	assert(
		tracker.serviceGeneration("Workspace") > genBefore,
		"R6: unexpected coupled Value change must also dirty Workspace so it is never silently swallowed"
	)

	local expected3 = { CFrame = { [partA] = roblox.CFrame.new(1, 2, 3) } }
	local active3: { any } = { partA, "CFrame", roblox.CFrame.new(1, 2, 3), nil }
	tracker.setBulkExpectedProperties(expected3, active3)
	assert(tracker.hasBulkSideEffectObserved() == false, "R6: new bulk expectation resets bulkSideEffectObserved")
	fireItemChanged(partB, "CFrame")
	assert(
		tracker.hasBulkSideEffectObserved() == true,
		"R6: unpatched cross-instance ItemChanged during activeBulkWrite must set bulkSideEffectObserved=true"
	)

	local desiredB = roblox.CFrame.new(4, 5, 6)
	partB.CFrame = desiredB
	local expected3b = { CFrame = { [partA] = roblox.CFrame.new(1, 2, 3), [partB] = desiredB } }
	tracker.setBulkExpectedProperties(expected3b, active3)
	fireItemChanged(partB, "CFrame")
	assert(
		tracker.hasBulkSideEffectObserved() == false,
		"R6: expected welded neighbour already at desired CFrame must not trigger false sideEffectObserved"
	)
	partB.CFrame = roblox.CFrame.new(9, 9, 9)
	fireItemChanged(partB, "CFrame")
	assert(
		tracker.hasBulkSideEffectObserved() == true,
		"R6: welded neighbour moved away from expected CFrame must set bulkSideEffectObserved=true"
	)

	tracker.setBulkExpectedProperties(nil)
	tracker.stop()
end

do
	local itemChangedCbs: { [number]: (any, any) -> () } = {}
	local addedCbs: { [number]: (any) -> () } = {}
	local notifyChangedCbs: { (any) -> () } = {}
	local nextId = 0
	local fakeClock = 100.0
	local onWaitCb: (() -> ())? = nil
	local relayNotify: any = nil
	local wsDescs: { any } = {}
	local fakeWs: any
	local fakeGame: any
	local function makeSig(tbl: { [number]: (any) -> () })
		return {
			Connect = function(_, fn)
				nextId += 1
				local id = nextId
				tbl[id] = fn
				return { Disconnect = function() tbl[id] = nil end }
			end,
		}
	end
	local tagsByInstance: { [any]: { string } } = {}
	local function makePart(name: string, parent: any, dbgId: string)
		local props: { [string]: any } = { Name = name, ClassName = "Part", Parent = parent, Archivable = true, Transparency = 0 }
		local obj: any
		obj = setmetatable({}, {
			__index = function(_, k)
				if k == "GetDebugId" then return function() return dbgId end end
				if k == "GetChildren" or k == "GetDescendants" then return function() return {} end end
				if k == "GetFullName" then return function() return "Workspace." .. props.Name end end
				if k == "GetAttributes" then return function() return {} end end
				if k == "GetTags" then return function() return table.clone(tagsByInstance[obj] or {}) end end
				if k == "IsA" then return function(_, c) return c == "Part" or c == "BasePart" end end
				if k == "IsDescendantOf" then return function(_, a) return props.Parent == a or (props.Parent ~= nil and a == fakeGame) end end
				if k == "AttributeChanged" or k == "Changed" then return { Connect = function() return { Disconnect = function() end } end } end
				return props[k]
			end,
			__newindex = function(_, k, v) props[k] = v end,
		})
		return obj
	end
	fakeWs = {
		Name = "Workspace", ClassName = "Workspace", Archivable = true,
		GetChildren = function() return wsDescs end,
		GetDescendants = function() return table.clone(wsDescs) end,
		GetDebugId = function() return "dbg-d2-ws" end,
		GetFullName = function() return "Workspace" end,
		GetAttributes = function() return {} end,
		GetTags = function() return table.clone(tagsByInstance[fakeWs] or {}) end,
		IsA = function(_, c) return c == "Workspace" end,
		IsDescendantOf = function(_, a) return a == fakeGame end,
		DescendantAdded = makeSig(addedCbs),
		DescendantRemoving = { Connect = function() return { Disconnect = function() end } end },
		AttributeChanged = { Connect = function() return { Disconnect = function() end } end },
		Changed = { Connect = function() return { Disconnect = function() end } end },
	}
	fakeGame = {
		ItemChanged = {
			Connect = function(_, fn)
				nextId += 1
				local id = nextId
				itemChangedCbs[id] = fn
				return { Disconnect = function() itemChangedCbs[id] = nil end }
			end,
		},
		GetService = function(_, name: string)
			if name == "Workspace" then return fakeWs end
			if name == "CoreGui" then return { Name = "CoreGui", ClassName = "CoreGui", GetChildren = function() return {} end } end
			if name == "HttpService" then return { GenerateGUID = function() return "d2-guid" end } end
			if name == "RunService" then return { Heartbeat = { Connect = function() return { Disconnect = function() end } end } } end
			if name == "CollectionService" then
				return {
					GetAllTags = function() return {} end,
					GetTagged = function() return {} end,
					GetTags = function(_, inst) return table.clone(tagsByInstance[inst] or {}) end,
					TagAdded = { Connect = function() return { Disconnect = function() end } end },
					TagRemoved = { Connect = function() return { Disconnect = function() end } end },
					GetInstanceAddedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
					GetInstanceRemovedSignal = function() return { Connect = function() return { Disconnect = function() end } end } end,
				}
			end
			if name == "ScriptEditorService" then
				return {
					GetScriptDocuments = function() return {} end, FindScriptDocument = function() return nil end,
					TextDocumentDidChange = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidOpen = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidClose = { Connect = function() return { Disconnect = function() end } end },
				}
			end
			return { GetChildren = function() return {} end, GetDescendants = function() return {} end }
		end,
	}
	fakeWs.Parent = fakeGame
	local d2Env: { [string]: any } = {
		game = fakeGame,
		typeof = function(v: any)
			if type(v) == "table" and v.ClassName ~= nil then return "Instance" end
			return typeof(v)
		end,
		Enum = roblox.Enum,
		Instance = {
			new = function(cls: string)
				if cls == "BindableEvent" then
					return { Event = { Connect = function() return { Disconnect = function() end } end }, Fire = function() end, Destroy = function() end }
				end
				if cls == "ObjectValue" then
					local obj: any = {
						Name = "ObjectValue", Archivable = true,
						Changed = { Connect = function(_, cb) notifyChangedCbs[#notifyChangedCbs + 1] = cb return { Disconnect = function() end } end },
						SetAttribute = function() end, Destroy = function() end,
					}
					relayNotify = obj
					return obj
				end
				return roblox.Instance.new(cls)
			end,
		},
		Vector3 = roblox.Vector3, CFrame = roblox.CFrame, Color3 = roblox.Color3,
		script = { Parent = setmetatable({}, { __index = function(_, n) return n end }) },
		os = { clock = function() return fakeClock end, time = os.time },
		task = {
			delay = function() end,
			defer = function(fn) fn() end,
			wait = function()
				if onWaitCb ~= nil then
					local cb = onWaitCb
					onWaitCb = nil
					cb()
				end
			end,
		},
	}
	local d2Mods: { [string]: any } = {
		RbxDom = {
			findCanonicalPropertyDescriptor = function(_, p) return { name = p, scriptability = "ReadWrite", dataType = "Float32" } end,
			getContentPropertyAliases = function() return {} end,
			getPropertyNames = function() return { "Name", "Transparency" } end,
			getReferencePropertyNames = function() return {} end,
		},
	}
	local function loadD2(n: string)
		if d2Mods[n] then return d2Mods[n] end
		local src = fs.readFile("tools/plugin_ws_bridge/" .. n .. ".module.luau"):gsub("\r\n", "\n")
		d2Mods[n] = luau.load(src, { environment = d2Env, debugName = n })()
		return d2Mods[n]
	end
	d2Env.require = loadD2
	local d2Tracker = loadD2("BridgeStudioChanges").create({
		LUA_SOURCE_CLASS = { Script = true, LocalScript = true, ModuleScript = true },
		bridgeRole = "edit", bridgeRuntimeId = "d2-tracker",
		shouldIgnoreInstance = function() return false end,
		exportInclusionUnder = function() return function() return true end end,
		invalidateRuntimeExportCache = function() end,
		loadPendingStudioChanges = function() return nil end,
		savePendingStudioChanges = function() return nil end,
	}, { Workspace = true })
	d2Tracker.configurePropertyCandidates({
		Part = { "Name", "Transparency", "Attachment0" },
		Workspace = { "Name", "Gravity" },
	})
	d2Tracker.getState({
		start = true, trackingGuardId = "d2-g", services = { "Workspace" },
		nativeAttributeRelay = true, deferNativeTracking = true, syncbackProperties = true,
	})
	for _, cb in ipairs(notifyChangedCbs) do cb(relayNotify) end
	d2Tracker.getState({ start = true, trackingGuardId = "d2-g", services = { "Workspace" }, syncbackProperties = true })

	local suspendedPart = makePart("SuspendedPart", fakeWs, "dbg-d2-susp")
	wsDescs = { suspendedPart }
	d2Tracker.beginChangeJournal("tx-d2-a", { "Workspace" })
	d2Tracker.beginNativeImportObservations("tx-d2-a", nil, { Workspace = true })
	local genBeforeD2a = d2Tracker.serviceGeneration("Workspace")
	suspendedPart.Transparency = 0.75
	for _, cb in pairs(table.clone(itemChangedCbs)) do
		cb(suspendedPart, "Transparency")
	end
	assert(
		d2Tracker.serviceGeneration("Workspace") > genBeforeD2a,
		"D2(a): ItemChanged on unconnected part in suspended service must mark Workspace dirty"
	)
	d2Tracker.finishNativeImportObservations("tx-d2-a", { ["dbg-d2-susp"] = "Part" })
	d2Tracker.finishChangeJournal("tx-d2-a")

	local preExistingPart = makePart("PreExistingPart", fakeWs, "dbg-f1-pre")
	wsDescs = { preExistingPart }
	d2Tracker.beginChangeJournal("tx-d2-b", { "Workspace" })
	d2Tracker.beginNativeImportObservations("tx-d2-b", nil, { Workspace = true })
	local genBeforeR1S = d2Tracker.serviceGeneration("Workspace")
	tagsByInstance[fakeWs] = { "RootTagDuringS" }
	for _, cb in pairs(table.clone(itemChangedCbs)) do
		cb(fakeWs, "Tags")
	end
	assert(
		d2Tracker.serviceGeneration("Workspace") > genBeforeR1S,
		"R1(S): tag edit on service root during suspended phase must mark Workspace dirty"
	)
	local scanParts = { preExistingPart }
	local scanCreated: { [string]: string } = {}
	for idx = 1, 2060 do
		local id = "dbg-d2-scan-" .. idx
		local p = makePart("P" .. idx, fakeWs, id)
		scanParts[#scanParts + 1] = p
		scanCreated[id] = "Part"
	end
	wsDescs = scanParts
	local latePart = scanParts[2056]
	local waitFired = false
	local f1RefDirty, f1TagDirty, r1ScanTagDirty, f2AttrDirty, d2bPropDirty = false, false, false, false, false
	fakeClock = 100.0
	onWaitCb = function()
		if waitFired then return end
		waitFired = true
		local g1 = d2Tracker.serviceGeneration("Workspace")
		preExistingPart.Attachment0 = scanParts[2]
		for _, cb in pairs(table.clone(itemChangedCbs)) do
			cb(preExistingPart, "Attachment0")
		end
		f1RefDirty = d2Tracker.serviceGeneration("Workspace") > g1
		local g2 = d2Tracker.serviceGeneration("Workspace")
		tagsByInstance[preExistingPart] = { "UserTagDuringScan" }
		for _, cb in pairs(table.clone(itemChangedCbs)) do
			cb(preExistingPart, "Tags")
		end
		f1TagDirty = d2Tracker.serviceGeneration("Workspace") > g2
		local gR1 = d2Tracker.serviceGeneration("Workspace")
		tagsByInstance[fakeWs] = { "RootTagDuringS", "RootTagDuringC" }
		for _, cb in pairs(table.clone(itemChangedCbs)) do
			cb(fakeWs, "Tags")
		end
		r1ScanTagDirty = d2Tracker.serviceGeneration("Workspace") > gR1
		local g3 = d2Tracker.serviceGeneration("Workspace")
		for _, cb in ipairs(notifyChangedCbs) do
			cb(latePart)
		end
		f2AttrDirty = d2Tracker.serviceGeneration("Workspace") > g3
		local g4 = d2Tracker.serviceGeneration("Workspace")
		latePart.Transparency = 0.42
		for _, cb in pairs(table.clone(itemChangedCbs)) do
			cb(latePart, "Transparency")
		end
		d2bPropDirty = d2Tracker.serviceGeneration("Workspace") > g4
	end
	local origClock = d2Env.os.clock
	d2Env.os.clock = function()
		fakeClock += 0.5
		return fakeClock
	end
	d2Tracker.finishNativeImportObservations("tx-d2-b", scanCreated)
	d2Env.os.clock = origClock
	assert(waitFired, "D2(b)/F1/F2/R1: scanSuspendedNativeService must have yielded via task.wait()")
	assert(f1RefDirty, "F1(ref): in-service reference edit on pre-existing part during scan yield must mark Workspace dirty")
	assert(f1TagDirty, "F1(tag): tag edit on pre-existing part during scan yield must mark Workspace dirty via markTagChange")
	assert(r1ScanTagDirty, "R1(C): tag edit on service root during scan yield must mark Workspace dirty")
	assert(f2AttrDirty, "F2(attr): attribute relay event on not-yet-scanned imported part during scan yield must mark Workspace dirty")
	assert(d2bPropDirty, "D2(b): ItemChanged on not-yet-scanned imported part during scan yield must mark Workspace dirty")
	d2Tracker.finishChangeJournal("tx-d2-b")

	wsDescs = {}
	d2Tracker.beginChangeJournal("tx-d2-c", { "Workspace" })
	d2Tracker.beginNativeImportObservations("tx-d2-c", nil, { Workspace = true })
	local incomingAtt = makePart("Att0", fakeWs, "dbg-d2-c-att")
	local incomingConstraint = makePart("Constraint0", fakeWs, "dbg-d2-c-con")
	incomingConstraint.Attachment0 = incomingAtt
	local incomingPart = makePart("NewPart", fakeWs, "dbg-d2-c-part")
	wsDescs = { incomingAtt, incomingConstraint, incomingPart }
	local genBeforeD2c = d2Tracker.serviceGeneration("Workspace")
	for _, cb in pairs(table.clone(itemChangedCbs)) do
		cb(incomingPart, "Parent")
		cb(incomingConstraint, "Attachment0")
	end
	assert(
		d2Tracker.serviceGeneration("Workspace") == genBeforeD2c,
		"D2(c): Parent and intra-tree Instance reference init on newly imported instances in suspended service must stay clean"
	)
	incomingPart.Transparency = 0.6
	for _, cb in pairs(table.clone(itemChangedCbs)) do
		cb(incomingPart, "Transparency")
	end
	assert(
		d2Tracker.serviceGeneration("Workspace") > genBeforeD2c,
		"D2(c): User property edit on newly imported (not pre-existing) instance in suspended service must mark Workspace dirty"
	)
	d2Tracker.finishNativeImportObservations("tx-d2-c", {
		["dbg-d2-c-att"] = "Part",
		["dbg-d2-c-con"] = "Part",
		["dbg-d2-c-part"] = "Part",
	})
	d2Tracker.finishChangeJournal("tx-d2-c")

	wsDescs = {}
	d2Tracker.beginChangeJournal("tx-det-ok", { "Workspace" })
	d2Tracker.beginNativeImportObservations("tx-det-ok", nil, {})
	local genDetOk = d2Tracker.serviceGeneration("Workspace")
	local detachedEngine = makePart("DetEngine", nil, "dbg-det-eng-rem")
	for _, cb in pairs(table.clone(addedCbs)) do cb(detachedEngine) end
	d2Tracker.finishNativeImportObservations("tx-det-ok", {}, { ["dbg-det-eng-rem"] = true }, nil)
	assert(
		d2Tracker.serviceGeneration("Workspace") == genDetOk,
		"Detached DescendantAdded with engine-removed debugId must stay clean"
	)
	d2Tracker.finishChangeJournal("tx-det-ok")

	d2Tracker.beginChangeJournal("tx-det-bad", { "Workspace" })
	d2Tracker.beginNativeImportObservations("tx-det-bad", nil, {})
	local genDetBad = d2Tracker.serviceGeneration("Workspace")
	local detachedUser = makePart("DetUser", nil, "dbg-det-user-unowned")
	for _, cb in pairs(table.clone(addedCbs)) do cb(detachedUser) end
	d2Tracker.finishNativeImportObservations("tx-det-bad", {}, nil, nil)
	assert(
		d2Tracker.serviceGeneration("Workspace") > genDetBad,
		"Detached DescendantAdded not in created/engine sets must mark Workspace dirty"
	)
	d2Tracker.finishChangeJournal("tx-det-bad")
	d2Tracker.stop()
end

do
	local fakeNow = 1000.0
	local delayed: { { at: number, fn: () -> () } } = {}
	local pinApi, workspace = buildReviewHarness({
		os = { clock = function() return fakeNow end, time = os.time },
		task = {
			wait = function() end,
			defer = function(fn) fn() end,
			delay = function(sec: number, fn: () -> ())
				delayed[#delayed + 1] = { at = fakeNow + sec, fn = fn }
			end,
		},
	})
	local part = roblox.Instance.new("Part")
	part.Name = "PinnedPart"
	part.Transparency = 0
	part.Parent = workspace
	local sB64 = base64Encode(buffer.fromstring("PinnedPart\0Part"))
	local nodesBuf = packNodes({ { 0, 1, 0, 1 } })
	pinApi.withRequestLease("l-pin-1", false, pinApi.beginTransaction, { transactionId = "tx-pin-1", services = { "Workspace" }, hasBulkPropertyPatches = true })
	local delayedBeforeApply = #delayed
	local p1Res = pinApi.withRequestLease("l-pin-1", false, pinApi.applyBulkProperties, {
		transactionId = "tx-pin-1", service = "Workspace",
		tree = { strings = sB64, stringCount = 2, nodeOffset = 0, nodeCount = 1, totalNodes = 1, cacheKey = "pin-k", nodes = nodesBuf },
		columns = { { propertyName = "Transparency", encoding = "f32_const", count = 1, rows = packU32({ 1 }), value = 0.2 } },
	})
	assert(p1Res.treeCacheHit ~= true, "Tree pin: first call populates cache")
	assert(
		#delayed > delayedBeforeApply,
		"Tree pin: populating persistentBulkTreeCache must schedule its own prune timer via task.delay (beyond armSessionExpiry)"
	)
	local pruneEntry = delayed[delayedBeforeApply + 1]
	pinApi.withRequestLease("l-pin-1", false, pinApi.commitTransaction, { transactionId = "tx-pin-1" })

	fakeNow += 61.0
	pruneEntry.fn()
	fakeNow -= 60.0
	pinApi.withRequestLease("l-pin-2", false, pinApi.beginTransaction, { transactionId = "tx-pin-2", services = { "Workspace" }, hasBulkPropertyPatches = true })
	local p2Res = pinApi.withRequestLease("l-pin-2", false, pinApi.applyBulkProperties, {
		transactionId = "tx-pin-2", service = "Workspace",
		tree = { strings = sB64, stringCount = 2, nodeOffset = 0, nodeCount = 1, totalNodes = 1, cacheKey = "pin-k", nodes = nodesBuf },
		columns = { { propertyName = "Transparency", encoding = "f32_const", count = 1, rows = packU32({ 1 }), value = 0.3 } },
	})
	assert(p2Res.treeCacheHit ~= true, "Tree pin: prune timer must physically evict persistentBulkTreeCache without relying on lookup expiry")
	pinApi.withRequestLease("l-pin-2", false, pinApi.commitTransaction, { transactionId = "tx-pin-2" })
end

print("Bulk plain-property workloads: tree ordinals, binary columns, cache reuse, rollback, ItemChanged fast path, PackageLink guard and review regressions passed")
