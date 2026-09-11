-- Run through the installed plugin in an explicitly selected disposable Edit place.
assert(game.PlaceId == 0 and game:GetService("RunService"):IsEdit(), "Needs an unpublished Edit fixture")
local trackerModule = require(script.BridgeStudioChanges)
local storage = game:GetService("ServerStorage")
local name = "ReniumValueObserverProbe"
assert(storage:FindFirstChild(name) == nil, "Probe root already exists")
local root = Instance.new("Folder")
root.Name = name
root.Parent = storage
local observer = trackerModule.create({
	bridgeRole = "edit", bridgeRuntimeId = "value-observer-probe", LUA_SOURCE_CLASS = {},
	loadPendingStudioChanges = function() end, savePendingStudioChanges = function() end,
	shouldIgnoreInstance = function() return false end, invalidateRuntimeExportCache = function() end,
}, { ServerStorage = true })
local activeFolder: Folder? = nil
local ok, result = pcall(function()
	observer.getState({ services = { "ServerStorage" } })
	for cycle = 1, 30 do
		observer.configurePropertyCandidates({ StringValue = { "Value", "Archivable" } })
		local folder = Instance.new("Folder")
		activeFolder = folder
		folder.Name = `Cycle{cycle}`
		local target: any = Instance.new("StringValue")
		target.Name, target.Value, target.Parent = "Before", "before", folder
		local destination = Instance.new("Folder")
		destination.Name, destination.Parent = "Destination", folder
		folder.Parent = root
		observer.beginSuppress()
		observer.beginChangeJournal("value-probe", { "ServerStorage" })
		local token = observer.expectParentChange(folder, nil)
		folder.Parent = nil
		observer.configurePropertyCandidates({ StringValue = { "Value", "Archivable", "Sandboxed" } })
		for _, change in { { "Name", "Outside", "Own" }, { "Archivable", false, true },
			{ "Parent", destination, folder }, { "Sandboxed", true, false } } do
			local property, outside, own = change[1], change[2], change[3]
			target[property] = outside
			observer.expectPropertyEvent(target, property, own)
			target[property] = own
		end
		local found
		for _, record in observer.finishChangeJournal("value-probe") do
			if record.instance == target then
				found = record
			end
		end
		assert(found and found.properties.Name and found.properties.Name.value == "Outside", "Lost outside name")
		assert(found.properties.Archivable and found.properties.Archivable.value == false, "Lost outside Archivable")
		assert(found.properties.Parent and found.properties.Parent.value == destination, "Lost outside parent")
		assert(found.properties.Sandboxed and found.properties.Sandboxed.value == true, "Lost configured property")
		observer.cancelExpectedEvent(token)
		observer.endSuppress(0)
		folder:Destroy()
		activeFolder = nil
	end
	return { passed = true, cycles = 30, installedModule = script.BridgeStudioChanges:GetFullName() }
end)
observer.stop()
if activeFolder then
	activeFolder:Destroy()
end
root:Destroy()
assert(ok, result)
assert(observer.getState({ start = false }).connectedInstances == 0, "Probe leaked instance observers")
return result
