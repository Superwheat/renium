local fs = require("@lune/fs")
local luau = require("@lune/luau")

-- Wide same-named sibling sets must cost one sibling scan per parent while a
-- traversal runs, and a scan again after any yield that can change the tree.
local mockGame = {}
local Identity = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeIdentity.module.luau"), {
	environment = { game = mockGame },
})()
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.luau")
local function extract(first, after, environment, prefix, returned)
	local start = assert(source:find(first, 1, true))
	local finish = assert(source:find(after, start, true))
	return luau.load(prefix .. source:sub(start, finish - 1) .. "\nreturn " .. returned, { environment = environment })()
end

local scans = {}
local onDescendants = {}
local function node(name, parent)
	local self = { Name = name, Parent = parent, Archivable = true, children = {} }
	function self:GetChildren()
		scans[self] = (scans[self] or 0) + 1
		return table.clone(self.children)
	end
	function self:GetDescendants()
		local out = {}
		local function visit(current)
			for _, child in current.children do
				out[#out + 1] = child
				visit(child)
			end
		end
		visit(self)
		if onDescendants[self] then
			onDescendants[self]()
		end
		return out
	end
	function self:IsDescendantOf(target)
		local current = self.Parent
		while current ~= nil do
			if current == target then
				return true
			end
			current = current.Parent
		end
		return false
	end
	if parent ~= nil and parent.children ~= nil then
		table.insert(parent.children, self)
	end
	return self
end
local function key(segments, ordinals)
	return Identity.pathCacheKey(segments, ordinals)
end

local count = 1500
local service = node("ServerStorage", mockGame)
local props = node("Props", service)
local models, parts = {}, {}
for index = 1, count do
	models[index] = node("Model", props)
	parts[index] = node("Part", models[index])
end

do
	local yieldPending = false
	local yields = 0
	local captureSnapshotRoots = extract("local function captureSnapshotRoots(", "local function captureMutationFingerprints(", {
		BridgeIdentity = Identity,
		BridgeParallel = { newWorkBudget = function()
			return function()
				if not yieldPending then
					return false
				end
				yieldPending = false
				yields += 1
				table.insert(props.children, 1, node("Model"))
				props.children[1].Parent = props
				return true
			end
		end },
		pathCacheKey = Identity.pathCacheKey,
	}, "", "captureSnapshotRoots")
	local half = count // 2
	onDescendants[models[half + 1]] = function()
		yieldPending = true
	end
	local originalByPath, originalRoots, nonArchivable, instanceCount =
		captureSnapshotRoots({ { target = props, preserved = {} } })
	assert(yields == 1 and #originalRoots == count and #nonArchivable == 0 and instanceCount == count * 2)
	assert(scans[props] == 3, `Expected the capture scan plus one sibling scan per snapshot, got {scans[props]}`)
	for index = 1, count do
		local ordinal = if index <= half then index else index + 1
		assert(originalByPath[key({ "ServerStorage", "Props", "Model" }, { 1, 1, ordinal })] == models[index],
			`Model {index} must map to live ordinal {ordinal} after the yield`)
		assert(originalByPath[key({ "ServerStorage", "Props", "Model", "Part" }, { 1, 1, ordinal, 1 })] == parts[index])
		assert(scans[models[index]] == 1, "Each model is scanned once for its part")
	end
	table.remove(props.children, 1)
	onDescendants[models[half + 1]] = nil
end

do
	scans = {}
	local appendJournalRecords = extract("local function resolveReplacement(", "function TransactionState.drainJournal(", {
		BridgeIdentity = Identity,
		CollectionService = { GetTags = function() return { "Tagged" } end },
	}, "local TransactionState = {}\n", "TransactionState.appendJournalRecords")
	local replaced = { Name = "Model", Parent = nil }
	local session = { instanceReplacements = { [replaced] = models[2] } }
	local records = { { instance = replaced, service = "ServerStorage", tagsChanged = true } }
	for index = 1, count do
		records[#records + 1] = { instance = parts[index], service = "ServerStorage" }
	end
	appendJournalRecords(session, records)
	assert(#session.changeJournal == count + 1, "Records are retained before their paths resolve")
	assert(records[1].currentInstance == models[2] and records[1].parent == props and records[1].tags[1] == "Tagged")
	assert(records[1].parentPathOrdinals[2] == 1 and #records[1].parentPathSegments == 2)
	for index = 1, count do
		local record = records[index + 1]
		assert(record.parent == models[index] and record.parentPathSegments[3] == "Model")
		assert(record.parentPathOrdinals[3] == index, `Record {index} must keep its parent ordinal`)
	end
	assert(scans[props] == 1, `Expected one sibling scan for {count} journal records, got {scans[props]}`)
	appendJournalRecords(session, { { instance = parts[1], service = "ServerStorage" } })
	assert(scans[props] == 2, "Each journal drain scans the live hierarchy afresh")
end
print("Path snapshots: original-instance capture refreshes after yields and journal records share sibling scans")
