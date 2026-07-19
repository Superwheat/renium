local BridgeValueEquality = {}

local EPSILON = 0.0001

local function numbersEqual(a: number, b: number): boolean
	if a ~= a and b ~= b then
		return true
	end
	return math.abs(a - b) < EPSILON
end

local function colorsEqual(a: Color3, b: Color3): boolean
	return math.floor(a.R * 255) == math.floor(b.R * 255)
		and math.floor(a.G * 255) == math.floor(b.G * 255)
		and math.floor(a.B * 255) == math.floor(b.B * 255)
end

local function vectorsEqual(a: any, b: any, fields: { string }): boolean
	for _, field in ipairs(fields) do
		if not numbersEqual(a[field], b[field]) then
			return false
		end
	end
	return true
end

local valuesEqual

local function tablesEqual(a: { [any]: any }, b: { [any]: any }, seen: { [any]: any }): boolean
	if seen[a] == b then
		return true
	end
	seen[a] = b
	for key, value in pairs(a) do
		if not valuesEqual(value, b[key], seen) then
			return false
		end
	end
	for key in pairs(b) do
		if a[key] == nil then
			return false
		end
	end
	return true
end

local function keypointsEqual(a: { any }, b: { any }, color: boolean): boolean
	if #a ~= #b then
		return false
	end
	for index, left in ipairs(a) do
		local right = b[index]
		if not numbersEqual(left.Time, right.Time) then
			return false
		end
		if color then
			if not colorsEqual(left.Value, right.Value) then
				return false
			end
		elseif not numbersEqual(left.Value, right.Value) or not numbersEqual(left.Envelope, right.Envelope) then
			return false
		end
	end
	return true
end

valuesEqual = function(a: any, b: any, seen: { [any]: any}?): boolean
	if a == b then
		return true
	end

	local typeA = typeof(a)
	local typeB = typeof(b)
	if typeA ~= typeB then
		if typeA == "number" and typeB == "EnumItem" then
			return a == b.Value
		elseif typeA == "EnumItem" and typeB == "number" then
			return a.Value == b
		end
		return false
	end

	if typeA == "number" then
		return numbersEqual(a, b)
	elseif typeA == "table" then
		return tablesEqual(a, b, seen or {})
	elseif typeA == "Color3" then
		return colorsEqual(a, b)
	elseif typeA == "Vector2" then
		return vectorsEqual(a, b, { "X", "Y" })
	elseif typeA == "Vector3" then
		return vectorsEqual(a, b, { "X", "Y", "Z" })
	elseif typeA == "CFrame" then
		local left = { a:GetComponents() }
		local right = { b:GetComponents() }
		for index, value in ipairs(left) do
			if not numbersEqual(value, right[index]) then
				return false
			end
		end
		return true
	elseif typeA == "UDim" then
		return numbersEqual(a.Scale, b.Scale) and numbersEqual(a.Offset, b.Offset)
	elseif typeA == "UDim2" then
		return valuesEqual(a.X, b.X) and valuesEqual(a.Y, b.Y)
	elseif typeA == "Rect" then
		return valuesEqual(a.Min, b.Min) and valuesEqual(a.Max, b.Max)
	elseif typeA == "NumberRange" then
		return numbersEqual(a.Min, b.Min) and numbersEqual(a.Max, b.Max)
	elseif typeA == "NumberSequence" then
		return keypointsEqual(a.Keypoints, b.Keypoints, false)
	elseif typeA == "ColorSequence" then
		return keypointsEqual(a.Keypoints, b.Keypoints, true)
	elseif typeA == "PhysicalProperties" then
		return vectorsEqual(a, b, {
			"Density",
			"Friction",
			"Elasticity",
			"FrictionWeight",
			"ElasticityWeight",
		})
	elseif typeA == "Ray" then
		return valuesEqual(a.Origin, b.Origin) and valuesEqual(a.Direction, b.Direction)
	elseif typeA == "Font" then
		return a.Family == b.Family and a.Weight == b.Weight and a.Style == b.Style
	end

	return false
end

BridgeValueEquality.valuesEqual = valuesEqual

return BridgeValueEquality
