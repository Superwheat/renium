local RenderQuality = {}

local properties = {
	{ "edit", "EditQualityLevel" },
	{ "play", "QualityLevel" },
}

-- RenderSettings are Studio-wide and persist when the Edit window closes, so
-- every override is remembered for restore and undone when the plugin unloads.
function RenderQuality.create(getSettings, getGameSettings, qualityLevels)
	local baseline = {}
	local written = {}
	local api = {}
	local levelsByValue = {}
	local maximum = 0
	for _, item in qualityLevels do
		levelsByValue[item.Value] = item
		maximum = math.max(maximum, item.Value)
	end

	local function read(service)
		local result = {}
		for _, property in properties do
			result[property[1]] = service[property[2]].Value
		end
		local gameOk, game = pcall(function()
			return getGameSettings().SavedQualityLevel.Value
		end)
		if gameOk and type(game) == "number" then
			result.game = game
		end
		return result
	end

	local function write(service, name, value)
		service[name] = levelsByValue[value]
		assert(service[name].Value == value, `Studio did not apply {name}`)
	end

	function api.restore()
		if next(baseline) == nil then
			return
		end
		local service = getSettings()
		for _, property in properties do
			local key, name = property[1], property[2]
			if baseline[key] ~= nil then
				if service[name].Value == written[key] then
					write(service, name, baseline[key])
				end
				baseline[key], written[key] = nil, nil
			end
		end
	end

	function api.handle(params)
		local action = params.action or "show"
		assert(action == "show" or action == "set" or action == "restore", "Unknown render quality action")
		local requested = params.values or {}
		assert(type(requested) == "table", "Render quality values must be an object")
		local count = 0
		for key, value in requested do
			local known = false
			for _, property in properties do
				if property[1] == key then
					known = true
					break
				end
			end
			assert(known, `Unknown render quality setting {key}`)
			assert(
				type(value) == "number" and value % 1 == 0 and levelsByValue[value] ~= nil,
				`{key} must be a whole number between 0 (automatic) and {maximum}`
			)
			count += 1
		end
		assert(if action == "set" then count > 0 else count == 0, "Only set accepts values; set needs at least one value")
		local service = getSettings()
		local before = read(service)
		if action == "restore" then
			api.restore()
			return { ok = true, settings = read(service), before = before, restored = true, maximum = maximum }
		end
		local changed = {}
		local ok, failure = pcall(function()
			for _, property in properties do
				local key, name = property[1], property[2]
				local value = requested[key]
				if value ~= nil and before[key] ~= value then
					changed[#changed + 1] = property
					write(service, name, value)
				end
			end
		end)
		if not ok then
			local restored, restoreError = pcall(function()
				for _, property in changed do
					write(service, property[2], before[property[1]])
				end
			end)
			error(if restored then tostring(failure) else `{failure}; rollback failed: {restoreError}`, 0)
		end
		local current = read(service)
		local changedNames = {}
		for _, property in changed do
			local key = property[1]
			if baseline[key] == nil then
				baseline[key] = before[key]
			end
			written[key] = current[key]
			changedNames[#changedNames + 1] = key
		end
		return { ok = true, settings = current, before = before, changed = changedNames, maximum = maximum }
	end
	return api
end

return RenderQuality
