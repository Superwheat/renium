-- Disposable native-import rollback qualification; no Play or saved-place writes.
assert(game.PlaceId == 0 and game:GetService("RunService"):IsEdit())
local editorModule = require(script.BridgeEditorSync)
local storage = game:GetService("ServerStorage")
local terrain = workspace.Terrain
local name = "ReniumNativeRollbackAudit"
assert(storage:FindFirstChild(name) == nil and storage:FindFirstChild(name .. "Reference") == nil
	and storage:FindFirstChild(name .. "CodeReference") == nil)
local holder = Instance.new("Folder")
holder.Name, holder.Parent = name, storage
local original = Instance.new("StringValue")
original.Name, original.Value, original.Parent = "Value", "before", holder
local originalCode = Instance.new("ModuleScript")
originalCode.Name, originalCode.Source, originalCode.Parent = "Code", "return 1\n", original
local reference = Instance.new("ObjectValue")
reference.Name, reference.Value, reference.Parent = name .. "Reference", original, storage
local codeReference = Instance.new("ObjectValue")
codeReference.Name, codeReference.Value, codeReference.Parent = name .. "CodeReference", originalCode, storage
game:GetService("RunService").Heartbeat:Wait()
local cell = Vector3int16.new(25000, 0, 25000)
local region = Region3.new(Vector3.new(100000, 0, 100000), Vector3.new(100008, 8, 100008))
local backup = terrain:CopyRegion(Region3int16.new(cell, Vector3int16.new(25001, 1, 25001)))

local tracker = require(script.BridgeStudioChanges).create({
	bridgeRole = "edit", bridgeRuntimeId = "native-rollback-audit", LUA_SOURCE_CLASS = { ModuleScript = true },
	loadPendingStudioChanges = function() end, savePendingStudioChanges = function() end,
	shouldIgnoreInstance = function() return false end, invalidateRuntimeExportCache = function() end,
}, { ServerStorage = true })
tracker.configurePropertyCandidates({ StringValue = { "Value", "Name", "Archivable" }, ObjectValue = { "Value" } })
local connected = tracker.getState({ services = { "ServerStorage" } }).connectedInstances
local ctx = {
	allowedServices = { ServerStorage = true }, luaSourceClass = { ModuleScript = true },
	stats = setmetatable({}, { __index = function() return 0 end }),
	assertSessionOwnership = function() end, invalidateService = function() end,
	includeExportInstance = function() return true end,
	finishEditorTransactionExpectation = function() end, updateStatus = function() end,
}
for key, method in pairs({
	expectPropertyEvent = "expectPropertyEvent", expectAttributeEvent = "expectAttributeEvent",
	expectParentChange = "expectParentChange", expectTagChange = "expectTagChange",
	cancelExpectedEvent = "cancelExpectedEvent", beginStudioChangeSuppression = "beginSuppress",
	endStudioChangeSuppression = "endSuppress", beginStudioChangeJournal = "beginChangeJournal",
	drainStudioChangeJournal = "drainChangeJournal", finishStudioChangeJournal = "finishChangeJournal",
	studioChangeGeneration = "serviceGeneration",
}) do ctx[key] = tracker[method] end
local api = editorModule.create(ctx)
local encoding = game:GetService("EncodingService")
local serialization = game:GetService("SerializationService")
local active, callback
local cases = {}
local ok, err = xpcall(function()
	for _, outsideValues in { "none", "incoming", "original" } do
		for _, reentrant in { false, true } do
			for _, delayed in { false, true } do
				original.Value = "before"
				originalCode.Source = "return 1\n"
				original:SetAttribute("Outside", nil)
				local wrapper = Instance.new("Folder")
				wrapper.Name = "payload"
				local incoming = Instance.new("StringValue")
				incoming.Name, incoming.Value, incoming.Parent = "__ReniumImportRoot_1", "own", wrapper
				local incomingCode = Instance.new("ModuleScript")
				incomingCode.Name, incomingCode.Source, incomingCode.Parent = "Code", "return 3\n", incoming
				local payload = serialization:SerializeInstancesAsync({ wrapper })
				wrapper:Destroy()
				terrain:FillBlock(region.CFrame, region.Size, Enum.Material.Grass)
				tracker.beginSuppress()
				active = `native-rollback:{reentrant}:{delayed}:{os.clock()}`
				assert(api.beginTransaction({ transactionId = active, services = { "ServerStorage" },
					nativeImport = true, nativeImportServices = { "ServerStorage" } }).ok)
				assert(api.beginBinaryImport({ transactionId = active, importId = active,
					totalBytes = buffer.len(payload), totalChunks = 1, instanceCount = 2,
					externalReferencesPostApplied = false, groups = {{ service = "ServerStorage",
						targetPath = { "ServerStorage", name }, count = 1, payloadRootName = "payload",
						rootPaths = {{ pathSegments = { "ServerStorage", name, "Value" },
							pathOrdinals = { 1, 1, 1 }, className = "StringValue" }} }} }).ok)
				assert(api.appendBinaryImport({ importId = active, index = 1,
					data = buffer.tostring(encoding:Base64Encode(payload)) }).ok)
				assert(api.finishBinaryImport({ importId = active }).ok)
				assert(api.commitTransaction({ transactionId = active, prepareOnly = true }).state == "prepared")
				assert(holder.Value ~= original and holder.Value.Value == "own")
				assert(reference.Value == holder.Value, "Forward native import did not retarget the external reference")
				assert(holder.Value.Code.Source == "return 3\n" and codeReference.Value == holder.Value.Code,
					"Forward native import lost the source or nested reference")
				if outsideValues ~= "none" then
					local changed = if outsideValues == "incoming" then holder.Value else original
					changed.Value = "outside"
					changed:SetAttribute("Outside", true)
					changed.Code.Source = "return 2\n"
				end
				terrain:FillBlock(region.CFrame, region.Size, Enum.Material.Snow)
				local callbackCount = 0
				if reentrant then
					callback = holder.ChildAdded:Connect(function(instance)
						if instance == original then
							callbackCount += 1
							terrain:FillBlock(region.CFrame, region.Size, Enum.Material.Rock)
						end
					end)
				end
				if delayed then game:GetService("RunService").Heartbeat:Wait() end
				assert(api.rollbackTransaction({ transactionId = active }).state == "rolledBack")
				active = nil
				tracker.endSuppress(0)
				if callback then callback:Disconnect(); callback = nil end
				local expectedValue = if outsideValues == "none" then "before" else "outside"
				assert(holder.Value == original and original.Value == expectedValue,
					`Rollback mismatch: {outsideValues}/{reentrant}/{delayed}; identity={holder.Value == original}; value={original.Value}`)
				assert(reference.Value == original, "Rollback did not restore the external reference")
				local expectedSource = if outsideValues == "none" then "return 1\n" else "return 2\n"
				assert(original.Code == originalCode and originalCode.Source == expectedSource and codeReference.Value == originalCode,
					"Rollback lost the outside source or nested reference")
				assert(original:GetAttribute("Outside") == if outsideValues == "none" then nil else true,
					"Rollback lost the outside attribute")
				local materials = terrain:ReadVoxelChannels(region, 4, { "SolidMaterial" }).SolidMaterial
				local expected = if reentrant then "Rock" else "Snow"
				assert(materials[1][1][1].Name == expected, "Rollback erased outside Terrain")
				local history = game:GetService("ChangeHistoryService")
				local canUndo, waypoint = history:GetCanUndo()
				assert(canUndo and string.sub(waypoint, 1, 28) == "Renium:Sync from filesystem:", tostring(waypoint))
				history:Undo()
				assert(holder:FindFirstChild("Value") == original and original.Value ~= "own",
					`Undo mismatch: {outsideValues}/{reentrant}/{delayed}; identity={holder:FindFirstChild("Value") == original}; value={original.Value}`)
				if outsideValues == "none" then assert(original.Value == "before") end
				local undoValue = original.Value
				assert(reference.Value == original, "Undo lost the external reference")
				assert(original.Code == originalCode and originalCode.Source ~= "return 3\n" and codeReference.Value == originalCode,
					"Undo revived the failed source or lost its reference")
				history:Redo()
				assert(holder:FindFirstChild("Value") == original and original.Value == expectedValue, "Redo revived a failed native import")
				assert(reference.Value == original, "Redo lost the external reference")
				assert(original.Code == originalCode and originalCode.Source == expectedSource and codeReference.Value == originalCode,
					"Redo revived the failed source or lost its reference")
				assert(original:GetAttribute("Outside") == if outsideValues == "none" then nil else true,
					"Redo lost the outside attribute")
				assert(terrain:ReadVoxelChannels(region, 4, { "SolidMaterial" }).SolidMaterial[1][1][1].Name == expected,
					"Redo did not preserve the accepted outside Terrain")
				assert(tracker.getState({ services = { "ServerStorage" } }).connectedInstances == connected,
					"Rollback leaked or lost instance observers")
				table.insert(cases, { reentrant = reentrant, delayed = delayed, callbackCount = callbackCount,
					expected = expected, actual = materials[1][1][1].Name, identityPreserved = true,
					undoRedo = true, outsideValues = outsideValues, referencePreserved = true, sourcePreserved = true, undoValue = undoValue })
			end
		end
	end
end, debug.traceback)
if active then pcall(api.rollbackTransaction, { transactionId = active }) end
if callback then callback:Disconnect() end
tracker.stop()
terrain:PasteRegion(backup, cell, true)
backup:Destroy()
holder:Destroy()
reference:Destroy()
codeReference:Destroy()
assert(ok, err)
return cases
