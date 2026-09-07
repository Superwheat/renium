local NetworkSimulation = {}

local properties = {
	{ "inDelay", "InboundNetworkMinDelayMs", 100 },
	{ "outDelay", "OutboundNetworkMinDelayMs", 100 },
	{ "inJitter", "InboundNetworkJitterMs", 100 },
	{ "outJitter", "OutboundNetworkJitterMs", 100 },
	{ "inLoss", "InboundNetworkLossPercent", 0.5 },
	{ "outLoss", "OutboundNetworkLossPercent", 0.5 },
}

local function near(a, b)
	return math.abs(a - b) <= 0.00001
end

-- NetworkSettings are process-local, not DataModel properties. Do not save settings
-- to disk or copy this state into a different play session.
function NetworkSimulation.create(getSettings)
	local baseline = {}
	local written = {}
	local api = {}

	local function read(service)
		local result = {}
		for _, property in properties do
			result[property[1]] = service[property[2]]
		end
		return result
	end

	function api.restore()
		if next(baseline) == nil then
			return
		end
		local service = getSettings()
		for _, property in properties do
			local key, name = property[1], property[2]
			if baseline[key] ~= nil then
				-- Do not undo a later manual change or another plugin's setting.
				if near(service[name], written[key]) then
					service[name] = baseline[key]
					assert(near(service[name], baseline[key]), `Studio did not restore {name}`)
				end
				baseline[key], written[key] = nil, nil
			end
		end
	end

	function api.handle(params)
		local action = params.action or "show"
		assert(action == "show" or action == "set" or action == "reset" or action == "restore", "Unknown network action")
		local requested = params.values or {}
		assert(type(requested) == "table", "Network values must be an object")
		local count = 0
		for key, value in requested do
			local maximum = nil
			for _, property in properties do
				if property[1] == key then
					maximum = property[3]
					break
				end
			end
			assert(maximum, `Unknown network setting {key}`)
			assert(type(value) == "number" and value == value and value >= 0 and value <= maximum,
				`{key} must be between 0 and {maximum}`)
			count += 1
		end
		assert(if action == "set" then count > 0 else count == 0, "Only set accepts values; set needs at least one value")
		local service = getSettings()
		local before = read(service)
		if action == "restore" then
			api.restore()
			return { ok = true, settings = read(service), before = before, restored = true }
		end
		local changed = {}
		-- An API/security error or engine clamping can occur on a different Studio build.
		local ok, failure = pcall(function()
			for _, property in properties do
				local key, name = property[1], property[2]
				local value = if action == "reset" then 0 else requested[key]
				if value ~= nil and not near(before[key], value) then
					changed[#changed + 1] = property
					service[name] = value
					assert(near(service[name], value), `Studio did not apply {name}`)
				end
			end
		end)
		if not ok then
			local restored, restoreError = pcall(function()
				for _, property in changed do
					service[property[2]] = before[property[1]]
					assert(near(service[property[2]], before[property[1]]), `Studio did not roll back {property[2]}`)
				end
			end)
			error(if restored then tostring(failure) else `{failure}; rollback failed: {restoreError}`, 0)
		end
		local current = read(service)
		local changedNames = {}
		for _, property in changed do
			local key = property[1]
			if params.client then
				if baseline[key] == nil then
					baseline[key] = before[key]
				end
				written[key] = current[key]
			end
			changedNames[#changedNames + 1] = key
		end
		return { ok = true, settings = current, before = before, changed = changedNames }
	end
	return api
end

return NetworkSimulation
