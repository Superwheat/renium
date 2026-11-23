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
				return { Get = function() return {} end }
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
					TextDocumentDidChange = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidOpen = { Connect = function() return { Disconnect = function() end } end },
					TextDocumentDidClose = { Connect = function() return { Disconnect = function() end } end },
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

print("Bulk plain-property workloads: tree ordinals, binary columns, cache reuse, rollback, ItemChanged fast path and PackageLink guard passed")
