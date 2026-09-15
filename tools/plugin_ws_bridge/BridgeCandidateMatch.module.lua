local BridgeCandidateMatch = {}

local MAX_CANDIDATES_TO_SCORE = 32

local function containsReference(value: any, seen: { [any]: boolean }?): boolean
	if type(value) ~= "table" then
		return false
	end
	if value._type == "Ref" or value.Ref ~= nil then
		return true
	end
	local visited = seen or {}
	if visited[value] then
		return false
	end
	visited[value] = true
	for key, nested in pairs(value) do
		if containsReference(key, visited) or containsReference(nested, visited) then
			return true
		end
	end
	return false
end

local function comparableCount(values: any): number
	if type(values) ~= "table" then
		return 0
	end
	local count = 0
	for _, value in pairs(values) do
		if not containsReference(value) then
			count += 1
		end
	end
	return count
end

local function scoreValues(candidate: any, values: any, compare: (any, string, any) -> boolean): number
	if type(values) ~= "table" then
		return 0
	end
	local score = 0
	for name, value in pairs(values) do
		if not containsReference(value) and compare(candidate, tostring(name), value) then
			score += 1
		end
	end
	return score
end

function BridgeCandidateMatch.choose(
	candidates: { any },
	properties: any,
	attributes: any,
	compareProperty: (any, string, any) -> boolean,
	compareAttribute: (any, string, any) -> boolean
): any
	if #candidates == 0 then
		return nil
	end
	if #candidates > MAX_CANDIDATES_TO_SCORE then
		return nil
	end

	local comparable = comparableCount(properties) + comparableCount(attributes)
	local best = nil
	local bestScore = 0
	local tied = false
	for _, candidate in ipairs(candidates) do
		local score = scoreValues(candidate, properties, compareProperty)
			+ scoreValues(candidate, attributes, compareAttribute)
		if score > bestScore then
			best = candidate
			bestScore = score
			tied = false
		elseif score == bestScore and score > 0 then
			tied = true
		end
	end
	if tied and bestScore < comparable then
		return nil
	end
	return best
end

return BridgeCandidateMatch
