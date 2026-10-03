local RenderQuality = require("../BridgeRenderQuality.module")

local levels = {}
for value = 0, 21 do
	levels[#levels + 1] = { Name = if value == 0 then "Automatic" else ("Level%02d"):format(value), Value = value }
end
local byValue = {}
for _, item in levels do
	byValue[item.Value] = item
end
local values = { EditQualityLevel = byValue[10], QualityLevel = byValue[21] }
local writes = 0
local reject = nil
local settings = setmetatable({}, {
	__index = values,
	__newindex = function(_, name, item)
		writes += 1
		if reject == name then
			error("permission denied")
		end
		values[name] = item
	end,
})
local gameLevel = { Value = 10 }
local api = RenderQuality.create(function()
	return settings
end, function()
	return { SavedQualityLevel = gameLevel }
end, levels)

local shown = api.handle({ action = "show" })
assert(shown.settings.edit == 10 and shown.settings.play == 21 and shown.settings.game == 10 and shown.maximum == 21)
assert(writes == 0, "show never writes")
for _, params in {
	{ action = "set", values = {} },
	{ action = "set", values = { edit = 22 } },
	{ action = "set", values = { edit = -1 } },
	{ action = "set", values = { edit = 1.5 } },
	{ action = "set", values = { edit = "21" } },
	{ action = "set", values = { game = 10 } },
	{ action = "show", values = { edit = 21 } },
	{ action = "restore", values = { play = 1 } },
} do
	assert(not pcall(api.handle, params), "invalid input must fail")
	assert(writes == 0, "invalid input must not change settings")
end

local raised = api.handle({ action = "set", values = { edit = 21 } })
assert(raised.settings.edit == 21 and raised.before.edit == 10 and raised.changed[1] == "edit" and #raised.changed == 1)
assert(values.EditQualityLevel.Value == 21 and values.QualityLevel.Value == 21)
local same = api.handle({ action = "set", values = { edit = 21, play = 21 } })
assert(#same.changed == 0 and writes == 1, "matching values are a no-op")
local lowered = api.handle({ action = "set", values = { play = 0 } })
assert(lowered.settings.play == 0 and values.QualityLevel.Name == "Automatic")

values.EditQualityLevel = byValue[5]
local restored = api.handle({ action = "restore" })
assert(restored.restored and restored.settings.edit == 5, "a later manual change is not undone by restore")
assert(restored.settings.play == 21, "an override still in place goes back to its baseline")
local again = api.handle({ action = "restore" })
assert(#again.settings == 0 and again.settings.play == 21, "restore without overrides changes nothing")

reject = "QualityLevel"
assert(not pcall(api.handle, { action = "set", values = { edit = 3, play = 7 } }))
assert(values.EditQualityLevel.Value == 5 and values.QualityLevel.Value == 21, "a failed write rolls back earlier writes")
reject = nil

api.handle({ action = "set", values = { edit = 1, play = 1 } })
api.restore()
assert(values.EditQualityLevel.Value == 5 and values.QualityLevel.Value == 21, "unload restore returns both baselines")
print("Renium render quality tests passed")
