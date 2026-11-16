local fs = require("@lune/fs")
local luau = require("@lune/luau")

-- A push review of a model whose children share one name (a rig's welds)
-- lists that model's children once for the whole review. It used to list them
-- twice per row, which kept Studio busy for half a minute on 850 welds.
local scans = 0
local function node(name, className)
	local children = {}
	return {
		Name = name,
		ClassName = className,
		children = children,
		GetChildren = function()
			scans += 1
			return children
		end,
	}
end
local workspace = node("Workspace", "Workspace")
local rig = node("Rig", "Model")
workspace.children[1] = rig
local WELDS = 900
for index = 1, WELDS do
	local weld = node("ManualWeld", "ManualWeld")
	weld.index = index
	rig.children[index] = weld
end

local ReviewModel = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeReviewModel.module.luau"), {
	environment = {
		game = {
			GetService = function(_, name)
				return if name == "Workspace" then workspace else {}
			end,
		},
	},
})()

local function groups(entry)
	local rows = {}
	for index = 1, WELDS do
		rows[index] = {
			service = "Workspace",
			pathSegments = { "Workspace", "Rig", "ManualWeld" },
			pathOrdinals = { 1, 1, index },
			className = "ManualWeld",
			entries = { { kind = "instanceAdd" }, entry },
		}
	end
	return rows
end

local pauses = 0
local indexSeen = nil
local helpers = {
	resolveInstance = function(_, childIndex)
		indexSeen = childIndex
		return nil
	end,
	decodeValue = function(raw)
		return true, raw
	end,
	readProperty = function(instance)
		return true, instance.index
	end,
	valuesEqual = function(left, right)
		return left == right
	end,
	yieldForBudget = function()
		pauses += 1
	end,
}

-- Every weld is in Studio with the value the files hold: nothing to review.
local started = os.clock()
local same = ReviewModel.buildTree({}, groups(nil), helpers)
local elapsed = os.clock() - started
assert(same.effectiveCount == 0, `expected no effective change, got {same.effectiveCount}`)
assert(scans <= 2, `the rig's children were listed {scans} times`)
assert(pauses == WELDS, `expected a pause check per row, got {pauses}`)
assert(type(indexSeen) == "table" and indexSeen[rig] ~= nil, "the resolver did not get the review's child index")
assert(elapsed < 2, `building the review took {elapsed} s`)

-- One changed value per weld: each row keeps its own live instance and shows
-- its ordinal, since the name is shared.
scans = 0
local changed = ReviewModel.buildTree({}, groups({ kind = "property", name = "Order", value = -1 }), helpers)
assert(changed.effectiveCount == WELDS, `expected {WELDS} changes, got {changed.effectiveCount}`)
assert(changed.instanceCount == WELDS, `expected {WELDS} instances, got {changed.instanceCount}`)
assert(scans <= 2, `the rig's children were listed {scans} times`)
local rigNode = changed.roots[1].children[1]
assert(rigNode.name == "Rig" and #rigNode.children == WELDS, "the rig lost rows")
for _, weld in ipairs(rigNode.children) do
	assert(weld.instance == rig.children[weld.ordinal], `row {weld.ordinal} shows another weld`)
	assert(weld.displayName == `ManualWeld[{weld.ordinal}]`, `row {weld.ordinal} is shown as {weld.displayName}`)
	assert(weld.props[1].oldText ~= nil, "the live value was not read")
end

-- A single child keeps its plain name, and a weld the place lacks is an addition.
local lone = ReviewModel.buildTree({}, {
	{
		service = "Workspace",
		pathSegments = { "Workspace", "Rig" },
		pathOrdinals = { 1, 1 },
		className = "Model",
		entries = { { kind = "property", name = "Order", value = -1 } },
	},
	{
		service = "Workspace",
		pathSegments = { "Workspace", "Rig", "ManualWeld" },
		pathOrdinals = { 1, 1, WELDS + 1 },
		className = "ManualWeld",
		entries = { { kind = "instanceAdd" } },
	},
}, helpers)
local loneRig = lone.roots[1].children[1]
assert(loneRig.displayName == "Rig", loneRig.displayName)
assert(loneRig.children[1].status == "added", "a weld Studio lacks was not shown as added")
assert(loneRig.children[1].displayName == `ManualWeld[{WELDS + 1}]`, loneRig.children[1].displayName)

print("review tree workloads passed")
