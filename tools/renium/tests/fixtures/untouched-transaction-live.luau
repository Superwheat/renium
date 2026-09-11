-- Opt-in regression in an unpublished disposable Edit fixture.
assert(game.PlaceId == 0 and game:GetService("RunService"):IsEdit())
local editorModule = require(script.BridgeEditorSync)
local terrain = workspace.Terrain
local storage = game:GetService("ServerStorage")
local name = "ReniumUntouchedTransactionAudit"
assert(storage:FindFirstChild(name) == nil)
local holder = Instance.new("Folder")
holder.Name, holder.Parent = name, storage
local value = Instance.new("StringValue")
value.Name, value.Value, value.Parent = "Value", "before", holder
local code = Instance.new("ModuleScript")
code.Name, code.Source, code.Parent = "Code", "return 1\n", holder
local cell = Vector3int16.new(25000, 0, 25000)
local region = Region3.new(Vector3.new(100000, 0, 100000), Vector3.new(100008, 8, 100008))
local backup = terrain:CopyRegion(Region3int16.new(cell, Vector3int16.new(25001, 1, 25001)))
local active
local tracker
local callback
local cases = {}
local ctx = {
	allowedServices = { ServerStorage = true },
	luaSourceClass = { ModuleScript = true },
	stats = {},
	assertSessionOwnership = function() end,
	studioChangeGeneration = function() return 0 end,
	beginStudioChangeSuppression = function() end,
	endStudioChangeSuppression = function() end,
	invalidateService = function() end,
	drainStudioChangeJournal = function() return {} end,
	finishStudioChangeJournal = function() return {} end,
	finishEditorTransactionExpectation = function() end,
}
local function writeOutside()
	value.Value = "outside"
	code.Source = "return 2\n"
	value:SetAttribute("Outside", true)
	local channels = terrain:ReadVoxelChannels(region, 4, { "SolidMaterial", "SolidOccupancy", "LiquidOccupancy" })
	for x = 1, 2 do
		for y = 1, 2 do
			for z = 1, 2 do
				channels.SolidMaterial[x][y][z] = Enum.Material.Snow
				channels.SolidOccupancy[x][y][z] = 0.5
				channels.LiquidOccupancy[x][y][z] = 0.75
			end
		end
	end
	terrain:WriteVoxelChannels(region, 4, {
		SolidMaterial = channels.SolidMaterial,
		SolidOccupancy = channels.SolidOccupancy,
		LiquidOccupancy = channels.LiquidOccupancy,
	})
end
local function readOutside()
	local channels = terrain:ReadVoxelChannels(region, 4, { "SolidMaterial", "SolidOccupancy", "LiquidOccupancy" })
	local voxels = {}
	for x = 1, 2 do
		for y = 1, 2 do
			for z = 1, 2 do
				table.insert(voxels, { channels.SolidMaterial[x][y][z].Name,
					channels.SolidOccupancy[x][y][z], channels.LiquidOccupancy[x][y][z] })
			end
		end
	end
	return { value = value.Value, source = code.Source, attribute = value:GetAttribute("Outside"), voxels = voxels }
end
local api = editorModule.create(ctx)
local ok, result = pcall(function()
	for _, failJournal in { false, true } do
		for _, delayed in { false, true } do
			value.Value = "before"
			code.Source = "return 1\n"
			value:SetAttribute("Outside", nil)
			terrain:FillBlock(region.CFrame, region.Size, Enum.Material.Grass)
			local before
			ctx.beginStudioChangeJournal = function()
				if failJournal then
					writeOutside()
					before = readOutside()
					error("expected journal startup failure")
				end
			end
			active = `untouched:{failJournal}:{delayed}:{os.clock()}`
			local began, response = pcall(api.beginTransaction, { transactionId = active, services = { "ServerStorage" } })
			if failJournal then
				assert(not began and tostring(response):find("expected journal startup failure", 1, true), tostring(response))
			else
				assert(began and response.ok and response.state == "open", tostring(response))
				writeOutside()
				before = readOutside()
			end
			if delayed then game:GetService("RunService").Heartbeat:Wait() end
			if not failJournal then assert(api.rollbackTransaction({ transactionId = active }).state == "rolledBack") end
			active = nil
			local after = readOutside()
			table.insert(cases, { failJournal = failJournal, delayed = delayed, before = before, after = after })
		end
	end
	tracker = require(script.BridgeStudioChanges).create({
		bridgeRole = "edit", bridgeRuntimeId = "property-rollback-audit", LUA_SOURCE_CLASS = { ModuleScript = true },
		loadPendingStudioChanges = function() end, savePendingStudioChanges = function() end,
		shouldIgnoreInstance = function() return false end, invalidateRuntimeExportCache = function() end,
	}, { ServerStorage = true })
	tracker.configurePropertyCandidates({ StringValue = { "Value", "Name", "Archivable" } })
	tracker.getState({ services = { "ServerStorage" } })
	for key, method in pairs({
		expectPropertyEvent = "expectPropertyEvent", expectAttributeEvent = "expectAttributeEvent",
		expectParentChange = "expectParentChange", expectTagChange = "expectTagChange",
		cancelExpectedEvent = "cancelExpectedEvent", beginStudioChangeSuppression = "beginSuppress",
		endStudioChangeSuppression = "endSuppress", beginStudioChangeJournal = "beginChangeJournal",
		drainStudioChangeJournal = "drainChangeJournal", finishStudioChangeJournal = "finishChangeJournal",
		studioChangeGeneration = "serviceGeneration",
	}) do ctx[key] = tracker[method] end
	ctx.stats = setmetatable({}, { __index = function() return 0 end })
	ctx.updateStatus = function() end
	for _, outsideValues in { false, true } do
		for _, reentrant in { false, true } do
			for _, delayed in { false, true } do
				value.Value = "before"
				code.Source = "return 1\n"
				value:SetAttribute("Outside", nil)
				terrain:FillBlock(region.CFrame, region.Size, Enum.Material.Grass)
				local change = { service = "ServerStorage", className = "StringValue",
					pathSegments = { "ServerStorage", name, "Value" }, pathOrdinals = { 1, 1, 1 },
					properties = { Value = "own" }, attributes = { Outside = "own" } }
				active = `property:{outsideValues}:{reentrant}:{delayed}:{os.clock()}`
				local sourceChange = { service = "ServerStorage", className = "ModuleScript",
					pathSegments = { "ServerStorage", name, "Code" }, pathOrdinals = { 1, 1, 1 }, source = "return 3\n" }
				tracker.beginSuppress()
				assert(api.beginTransaction({ transactionId = active, services = { "ServerStorage" },
					propertyChanges = { change }, sourceChanges = { sourceChange } }).ok)
				if outsideValues then writeOutside() else terrain:FillBlock(region.CFrame, region.Size, Enum.Material.Snow) end
				local before = readOutside()
				local applied = api.applyChanges({ transactionId = active,
					propertyChanges = { change }, sourceChanges = { sourceChange } })
				assert(applied.ok and value.Value == "own" and code.Source == "return 3\n", tostring(applied.error))
				if reentrant then
					callback = value:GetPropertyChangedSignal("Value"):Connect(function()
						if value.Value == "before" then
							terrain:FillBlock(region.CFrame, region.Size, Enum.Material.Rock)
							before.voxels = readOutside().voxels
						end
					end)
				end
				if delayed then game:GetService("RunService").Heartbeat:Wait() end
				assert(api.rollbackTransaction({ transactionId = active }).state == "rolledBack")
				tracker.endSuppress(0)
				active = nil
				if callback then callback:Disconnect(); callback = nil end
				local after = readOutside()
				local history = game:GetService("ChangeHistoryService")
				local canUndo, waypoint = history:GetCanUndo()
				assert(canUndo and string.sub(waypoint, 1, 28) == "Renium:Sync from filesystem:", tostring(waypoint))
				history:Undo()
				assert(value.Value ~= "own" and code.Source ~= "return 3\n", "Undo revived failed own values")
				if not outsideValues then
					assert(value.Value == "before" and code.Source == "return 1\n", "Own-only undo changed original values")
				end
				history:Redo()
				assert(editorModule.valuesEqual(after, readOutside()), "Redo revived failed own values")
				table.insert(cases, { propertyOnly = true, outsideValues = outsideValues, reentrant = reentrant,
					delayed = delayed, before = before, after = after, undoRedo = true })
			end
		end
	end
end)
if active then pcall(api.rollbackTransaction, { transactionId = active }) end
if callback then callback:Disconnect() end
if tracker then tracker.stop() end
terrain:PasteRegion(backup, cell, true)
backup:Destroy()
holder:Destroy()
assert(ok, result)
for _, case in ipairs(cases) do
	assert(editorModule.valuesEqual(case.before, case.after), "Untouched transaction erased outside edits")
end
return cases
