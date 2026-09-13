assert(game.PlaceId == 0 and game:GetService("RunService"):IsEdit())
assert(settings():GetFFlag("RemovePackageModificationPopupDialog"), "Native popup suppression is inactive")
local root = game:GetService("ServerStorage").Public
local link = assert(root:FindFirstChildWhichIsA("PackageLink"))
local parent, count = root.Parent, #root:GetDescendants()
local before = root:GetAttribute("ReniumNoticeProbe")
local tracker = require(script.BridgeStudioChanges).create({
	bridgeRole = "edit", bridgeRuntimeId = "package-notice-audit", LUA_SOURCE_CLASS = {},
	loadPendingStudioChanges = function() end, savePendingStudioChanges = function() end,
	shouldIgnoreInstance = function() return false end, invalidateRuntimeExportCache = function() end,
}, { ServerStorage = true })
local ctx = {
	allowedServices = { ServerStorage = true }, luaSourceClass = {},
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
local api = require(script.BridgeEditorSync).create(ctx)
local active
local ok, err = xpcall(function()
	for _, cleanup in { false, true } do
		active = `package-notice:{cleanup}:{os.clock()}`
		local change = {
			service = "ServerStorage", pathSegments = { "ServerStorage", "Public" },
			pathOrdinals = { 1, 1 }, className = root.ClassName, properties = {},
			attributes = { ReniumNoticeProbe = `probe:{os.clock()}` },
		}
		assert(api.beginTransaction({ transactionId = active, services = { "ServerStorage" },
			propertyChanges = { change } }).ok, "Could not begin transaction")
		assert(api.applyChanges({ transactionId = active, services = { "ServerStorage" },
			propertyChanges = { change } }).ok, "Could not apply package edit")
		assert(root:GetAttribute("ReniumNoticeProbe") ~= before, "Package edit was not applied")
		if cleanup then api.cleanup() else api.rollbackTransaction({ transactionId = active }) end
		assert(api.getTransactionState({ transactionId = active }).state == "rolledBack", "Rollback failed")
		active = nil
		assert(root:GetAttribute("ReniumNoticeProbe") == before, "Rollback lost the original attribute")
		assert(root.Parent == parent and root:FindFirstChildWhichIsA("PackageLink") == link,
			"Rollback changed the package link or parent")
		assert(#root:GetDescendants() == count, "Rollback changed the package hierarchy")
	end
	task.wait(2)
end, debug.traceback)
if active then api.rollbackTransaction({ transactionId = active }) end
tracker.stop()
assert(ok, err)
return { rollback = true, cleanup = true, linkRetained = true, popupSuppressed = true }
