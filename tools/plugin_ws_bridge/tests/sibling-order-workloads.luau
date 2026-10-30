local fs = require("@lune/fs")
local luau = require("@lune/luau")

-- A large upsert reaches the plugin in 5,000-entry batches that share no
-- resolved-entry map: the later batches find their parents live, by name and
-- sibling ordinal. The sorted order must therefore create same-named siblings
-- in ordinal order, so the live order matches the ordinals the store assigned.
local mockGame = {}
local Identity = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeIdentity.module.luau"), {
	environment = { game = mockGame },
})()
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.luau")
local function extract(first, after, environment, returned)
	local start = assert(source:find(first, 1, true))
	local finish = assert(source:find(after, start, true))
	return luau.load(source:sub(start, finish - 1) .. "\nreturn " .. returned, { environment = environment })()
end
local cloneArray = extract("local function cloneArray(", "local function denseArrayLength(", {}, "cloneArray")
local sortedInstanceEntries = extract("local function entryApplyOrder(", "local function syncDesiredEntry(", {
	cloneArray = cloneArray,
	pathKey = Identity.pathKey,
	pathCacheKey = Identity.pathCacheKey,
	settingsIdText = function(raw) return if type(raw) == "string" then raw else nil end,
}, "sortedInstanceEntries")

local function node(name)
	local children = {}
	return {
		Name = name,
		children = children,
		GetChildren = function() return children end,
		FindFirstChild = function(_, childName)
			for _, child in children do
				if child.Name == childName then
					return child
				end
			end
			return nil
		end,
	}
end
local service = node("ServerStorage")
mockGame.GetService = function(_, name)
	assert(name == "ServerStorage", name)
	return service
end

local rows = {}
local function row(segments, ordinals, className)
	rows[#rows + 1] = {
		pathSegments = segments,
		pathOrdinals = ordinals,
		className = className,
		settingsId = string.format("%x", #rows + 1),
	}
end
local props = { "ServerStorage", "Maps", "Village", "Props" }
row({ "ServerStorage", "Maps" }, { 1, 1 }, "Folder")
row({ "ServerStorage", "Maps", "Village" }, { 1, 1, 1 }, "Model")
row(props, { 1, 1, 1, 1 }, "Model")
-- Twelve same-named siblings: ordinals 10..12 sort before 2 as text. Each
-- holds a different number of same-named parts so the live order matters.
for model = 1, 12 do
	row({ "ServerStorage", "Maps", "Village", "Props", "Model" }, { 1, 1, 1, 1, model }, "Model")
	for part = 1, (model % 3) + 1 do
		row({ "ServerStorage", "Maps", "Village", "Props", "Model", "Part" }, { 1, 1, 1, 1, model, part }, "Part")
		row(
			{ "ServerStorage", "Maps", "Village", "Props", "Model", "Part", "Texture" },
			{ 1, 1, 1, 1, model, part, 1 },
			"Texture"
		)
	end
	row({ "ServerStorage", "Maps", "Village", "Props", "Model", "Model" }, { 1, 1, 1, 1, model, 1 }, "Model")
end
local shuffled = table.clone(rows)
for index = #shuffled, 2, -1 do
	local swap = (index * 7919) % index + 1
	shuffled[index], shuffled[swap] = shuffled[swap], shuffled[index]
end

local entries = sortedInstanceEntries({ instances = shuffled }, "ServerStorage")
assert(#entries == #rows, `Expected {#rows} entries, got {#entries}`)

local function parentParts(entry)
	local segments, ordinals = {}, {}
	for i = 1, #entry.pathSegments - 1 do
		segments[i] = entry.pathSegments[i]
		ordinals[i] = entry.pathOrdinals[i]
	end
	return segments, ordinals
end

local seen = {}
local created = {}
local batchBoundary = math.floor(#entries / 2)
for index, entry in entries do
	local parentSegments, parentOrdinals = parentParts(entry)
	local parentKey = Identity.pathCacheKey(parentSegments, parentOrdinals)
	if #parentSegments > 1 then
		assert(seen[parentKey], `{entry.key} arrived before its parent`)
	end
	-- The first batch keeps the parents it created; the second batch only
	-- has the live tree.
	local parent = if index <= batchBoundary then created[parentKey] else nil
	parent = parent or Identity.resolvePathSegments(parentSegments, nil, parentOrdinals)
	assert(parent ~= nil, `Live lookup found no parent for {entry.key}`)
	local instance = node(entry.pathSegments[#entry.pathSegments])
	instance.Parent = parent
	parent.children[#parent.children + 1] = instance
	seen[entry.key] = true
	created[entry.key] = instance
end

for _, entry in entries do
	local live = Identity.resolvePathSegments(entry.pathSegments, nil, entry.pathOrdinals)
	assert(live == created[entry.key], `Live ordinals of {entry.key} name another sibling`)
end
print("Sibling order: same-named siblings are created in ordinal order so later batches resolve parents live")
