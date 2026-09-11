-- Exercise the installed tracker with a foreign callback preceding its add callback.
assert(game.PlaceId == 0 and game:GetService("RunService"):IsEdit(), "Needs an unpublished Edit fixture")
local trackerModule = require(script.BridgeStudioChanges)
local storage = game:GetService("ServerStorage")
local name = "ReniumNativeParentProbe"
assert(storage:FindFirstChild(name) == nil, "Probe root already exists")
local activeRoot, activeObserver, foreignConnection
local cases = 0
local ok, result = pcall(function()
	for _, mode in { "unchanged", "own-only", "outside", "outside-then-own" } do
		for _ = 1, 15 do
			local root = Instance.new("Folder")
			activeRoot = root
			root.Name = name
			local first = Instance.new("Folder")
			first.Name, first.Parent = "First", root
			local later = Instance.new("Part")
			later.Name, later.Transparency, later.Parent = "Later", 0.25, root
			later:SetAttribute("Keep", "before")
			later:SetAttribute("Delete", true)
			later:SetAttribute("Vector", Vector3.new(1, 2, 3))
			local observer = trackerModule.create({
				bridgeRole = "edit", bridgeRuntimeId = "native-parent-probe", LUA_SOURCE_CLASS = {},
				loadPendingStudioChanges = function() end, savePendingStudioChanges = function() end,
				shouldIgnoreInstance = function() return false end, invalidateRuntimeExportCache = function() end,
			}, { ServerStorage = true })
			activeObserver = observer
			observer.configurePropertyCandidates({ Part = { "Transparency" } })
			local foreignCalls = 0
			foreignConnection = storage.DescendantAdded:Connect(function(instance)
				if instance ~= first then return end
				foreignCalls += 1
				if mode == "unchanged" then
					later:SetAttribute("Keep", "before")
					later:SetAttribute("Delete", true)
					later:SetAttribute("Vector", Vector3.new(1, 2, 3))
				end
				if mode == "outside" or mode == "outside-then-own" then
					later.Transparency = 0.5
					later:SetAttribute("Keep", false)
					later:SetAttribute("Delete", nil)
					later:SetAttribute("Vector", Vector3.new(4, 5, 6))
				end
				if mode == "own-only" or mode == "outside-then-own" then
					observer.expectPropertyEvent(later, "Transparency", 0.75)
					later.Transparency = 0.75
					for _, attribute in { "Keep", "Delete", "Vector" } do
						observer.expectAttributeEvent(later, attribute, "own")
						later:SetAttribute(attribute, "own")
					end
				end
			end)
			observer.getState({ services = { "ServerStorage" } })
			observer.beginSuppress()
			observer.beginChangeJournal("native-parent", { "ServerStorage" })
			local token = observer.expectParentChange(root, storage, {}, true)
			root.Parent = storage
			if token.complete then
				token.complete()
			end
			local records = observer.finishChangeJournal("native-parent")
			assert(foreignCalls == 1, "Foreign insertion callback did not execute")
			if mode == "unchanged" or mode == "own-only" then
				assert(#records == 0, `Own/unchanged insertion produced outside edits: {mode}`)
			else
				assert(#records == 1 and records[1].instance == later, "Lost outside instance identity")
				local record = records[1]
				assert(record.properties.Transparency.value == 0.5, "Lost outside property")
				assert(record.attributes.Keep.captured and record.attributes.Keep.value == false, "Lost false attribute")
				assert(record.attributes.Delete.captured and record.attributes.Delete.value == nil, "Lost attribute deletion")
				assert(record.attributes.Vector.value == Vector3.new(4, 5, 6), "Lost typed attribute")
			end
			observer.cancelExpectedEvent(token)
			observer.endSuppress(0)
			foreignConnection:Disconnect()
			foreignConnection = nil
			observer.stop()
			assert(observer.getState({ start = false }).connectedInstances == 0, "Leaked instance observers")
			activeObserver = nil
			root:Destroy()
			activeRoot = nil
			cases += 1
		end
	end
	return { passed = true, cases = cases, installedModule = script.BridgeStudioChanges:GetFullName() }
end)
if foreignConnection then
	foreignConnection:Disconnect()
end
if activeObserver then
	activeObserver.stop()
end
if activeRoot then
	activeRoot:Destroy()
end
assert(ok, result)
return result
