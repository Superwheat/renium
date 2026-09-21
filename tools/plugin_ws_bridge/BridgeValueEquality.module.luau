local BridgeValueEquality = {}

-- Match the host's f32 readback bounds; never quantize float colors to bytes.
local FLOAT32_EPSILON = 2 ^ -23
local VECTOR2_FIELDS = { "X", "Y" }
local VECTOR3_FIELDS = { "X", "Y", "Z" }
local PHYSICAL_PROPERTIES_FIELDS = {
	"Density",
	"Friction",
	"Elasticity",
	"FrictionWeight",
	"ElasticityWeight",
}

local function numbersEqual(a: number, b: number): boolean
	if a == b then
		return true
	end
	if a ~= a and b ~= b then
		return true
	end
	return math.abs(a) < math.huge and math.abs(b) < math.huge
		and math.abs(a - b) <= 4 * FLOAT32_EPSILON * math.max(1, math.abs(a), math.abs(b))
end

local function colorsEqual(a: Color3, b: Color3): boolean
	return numbersEqual(a.R, b.R) and numbersEqual(a.G, b.G) and numbersEqual(a.B, b.B)
end

local function vectorsEqual(a: any, b: any, fields: { string }): boolean
	for _, field in ipairs(fields) do
		if not numbersEqual(a[field], b[field]) then
			return false
		end
	end
	return true
end

local function contentEqualsString(content: Content, text: string): boolean
	if content.SourceType == Enum.ContentSourceType.None then
		return text == ""
	elseif content.SourceType == Enum.ContentSourceType.Uri then
		return content.Uri == text
	end
	return false
end

local valuesEqual
local exactValuesEqual

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

local function exactTablesEqual(a: { [any]: any }, b: { [any]: any }, seen: { [any]: any }): boolean
	if seen[a] == b then
		return true
	end
	seen[a] = b
	for key, value in pairs(a) do
		if not exactValuesEqual(value, b[key], seen) then
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

valuesEqual = function(a: any, b: any, seen: { [any]: any }?): boolean
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
		elseif typeA == "Content" and typeB == "string" then
			return contentEqualsString(a, b)
		elseif typeA == "string" and typeB == "Content" then
			return contentEqualsString(b, a)
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
		return vectorsEqual(a, b, VECTOR2_FIELDS)
	elseif typeA == "Vector3" then
		return vectorsEqual(a, b, VECTOR3_FIELDS)
	elseif typeA == "CFrame" then
		local left = { a:GetComponents() }
		local right = { b:GetComponents() }
		for index, value in ipairs(left) do
			local other = right[index]
			local equal = if index <= 3 then numbersEqual(value, other)
				else value == other or value ~= value and other ~= other
					or math.abs(value) < math.huge and math.abs(other) < math.huge
						and math.abs(value - other) <= 16 * FLOAT32_EPSILON
			if not equal then
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
		local baseEqual = vectorsEqual(a, b, PHYSICAL_PROPERTIES_FIELDS)
		if not baseEqual then
			return false
		end
		local okAcousticA, acousticA = pcall(function()
			return a.AcousticAbsorption
		end)
		local okAcousticB, acousticB = pcall(function()
			return b.AcousticAbsorption
		end)
		if okAcousticA ~= okAcousticB then
			return false
		end
		return not okAcousticA or numbersEqual(acousticA, acousticB)
	elseif typeA == "Ray" then
		return valuesEqual(a.Origin, b.Origin) and valuesEqual(a.Direction, b.Direction)
	elseif typeA == "Font" then
		return a.Family == b.Family and a.Weight == b.Weight and a.Style == b.Style
	end

	return false
end

exactValuesEqual = function(a: any, b: any, seen: { [any]: any }?): boolean
	if a == b then
		return true
	end
	if typeof(a) == "number" and typeof(b) == "number" then
		return a ~= a and b ~= b
	end
	if typeof(a) == "CFrame" and typeof(b) == "CFrame" then
		return exactTablesEqual({ a:GetComponents() }, { b:GetComponents() }, {})
	end
	if type(a) ~= "table" or type(b) ~= "table" then
		return false
	end
	return exactTablesEqual(a, b, seen or {})
end

BridgeValueEquality.valuesEqual = valuesEqual
BridgeValueEquality.exactValuesEqual = exactValuesEqual

return BridgeValueEquality
