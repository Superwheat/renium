local EncodingService = game:GetService("EncodingService")

local BridgeValueCodec = {}

function BridgeValueCodec.encodeNumber(value)
	if type(value) ~= "number" then
		return nil
	end
	if value ~= value then
		return { _type = "Float", value = "nan" }
	elseif value == math.huge then
		return { _type = "Float", value = "inf" }
	elseif value == -math.huge then
		return { _type = "Float", value = "-inf" }
	end
	return value
end

function BridgeValueCodec.decodeNumber(value)
	if type(value) == "table" and value._type == "Float" then
		if value.value == "nan" then
			return 0 / 0
		elseif value.value == "inf" then
			return math.huge
		elseif value.value == "-inf" then
			return -math.huge
		end
		return nil
	end
	return tonumber(value)
end

function BridgeValueCodec.numbersEqual(left, right)
	local leftNumber = BridgeValueCodec.decodeNumber(left)
	local rightNumber = BridgeValueCodec.decodeNumber(right)
	if not leftNumber or not rightNumber then
		return false
	end
	if leftNumber ~= leftNumber then
		return rightNumber ~= rightNumber
	end
	return leftNumber == rightNumber
end

function BridgeValueCodec.encodeComponents(...)
	local values = table.pack(...)
	local count = values.n
	values.n = nil
	for index = 1, count do
		local encoded = BridgeValueCodec.encodeNumber(values[index])
		if not encoded then
			error("Numeric component encoder received a non-number")
		end
		values[index] = encoded
	end
	return values
end

BridgeValueCodec.encodeTransportNumber = BridgeValueCodec.encodeNumber
BridgeValueCodec.encodeTransportComponents = BridgeValueCodec.encodeComponents

function BridgeValueCodec.configureNativeNonFiniteJson(HttpService)
	local encodeOk, encoded = pcall(HttpService.JSONEncode, HttpService, {
		0 / 0,
		math.huge,
		-math.huge,
	})
	if
		not encodeOk
		or type(encoded) ~= "string"
		or not string.find(encoded, '"t":"numeric"', 1, true)
		or not string.find(encoded, '"v":"nan"', 1, true)
		or not string.find(encoded, '"v":"inf"', 1, true)
		or not string.find(encoded, '"v":"-inf"', 1, true)
	then
		return false
	end
	local decodeOk, decoded = pcall(HttpService.JSONDecode, HttpService, encoded)
	if
		not decodeOk
		or type(decoded) ~= "table"
		or decoded[1] == decoded[1]
		or decoded[2] ~= math.huge
		or decoded[3] ~= -math.huge
	then
		return false
	end
	BridgeValueCodec.encodeTransportNumber = function(value)
		return if type(value) == "number" then value else nil
	end
	BridgeValueCodec.encodeTransportComponents = function(...)
		return { ... }
	end
	return true
end

local function decodeNumberFields(raw: any, fields: { any }): (boolean, any)
	if type(raw) ~= "table" then
		return false, "Typed numeric value must be an object"
	end
	local values = table.create(#fields)
	for index, field in ipairs(fields) do
		local names = if type(field) == "table" then field else { field }
		local rawValue = nil
		for _, name in ipairs(names) do
			if raw[name] ~= nil then
				rawValue = raw[name]
				break
			end
		end
		local value = BridgeValueCodec.decodeNumber(rawValue)
		if not value then
			return false, tostring(names[1]) .. " must be a number"
		end
		values[index] = value
	end
	return true, values
end

local function decodeColor3(raw: any): (boolean, any)
	if typeof(raw) == "Color3" then
		return true, raw
	end
	local ok, values = decodeNumberFields(raw, {
		{ "r", "R", 1 },
		{ "g", "G", 2 },
		{ "b", "B", 3 },
	})
	if not ok then
		return false, "Color3 " .. tostring(values)
	end
	return true, Color3.new(values[1], values[2], values[3])
end

local function decodeEnumItem(raw: { [string]: any }, enumHint: string?): (boolean, any)
	local rawEnumType = tostring(raw.enumType or "")
	local enumType = if rawEnumType ~= ""
			or not enumHint
			or enumHint == ""
		then rawEnumType
		elseif string.sub(enumHint, 1, 5) == "Enum." then enumHint
		else "Enum." .. enumHint
	local enumName = string.gsub(enumType, "^Enum%.", "")
	local itemName = tostring(raw.name or "")
	local ok, item = pcall(function()
		return (Enum :: any)[enumName][itemName]
	end)
	if ok and item ~= nil then
		return true, item
	end
	return false, ("Unknown enum item %s.%s"):format(enumType, itemName)
end

function BridgeValueCodec.decode(raw: any, enumHint: string?, decodeRef, context, serviceName): (boolean, any)
	if type(raw) ~= "table" then
		return true, raw
	end

	local typeName = raw._type
	if typeName == nil and enumHint == "FontFace" and raw.family ~= nil then
		typeName = "Font"
	elseif typeName == nil and raw.BrickColor ~= nil then
		typeName = "BrickColor"
	elseif typeName == nil and type(raw.ColorSequence) == "table" then
		raw = raw.ColorSequence
		typeName = "ColorSequence"
	elseif typeName == nil and type(raw.NumberSequence) == "table" then
		raw = raw.NumberSequence
		typeName = "NumberSequence"
	elseif typeName == nil and type(raw.NumberRange) == "table" then
		raw = raw.NumberRange
		typeName = "NumberRange"
	elseif typeName == nil and type(raw.Ref) == "table" then
		raw = raw.Ref
		typeName = "Ref"
	elseif typeName == nil and raw.customPhysics ~= nil then
		typeName = "PhysicalProperties"
	end
	if typeName == nil then
		return true, raw
	end
	typeName = tostring(typeName)

	if typeName == "Float" then
		local value = BridgeValueCodec.decodeNumber(raw)
		if not value then
			return false, "Float value must be a number or non-finite marker"
		end
		return true, value
	elseif typeName == "BinaryString" then
		local encoded = raw.base64
		if type(encoded) ~= "string" then
			return false, "BinaryString base64 must be a string"
		end
		local ok, decoded = pcall(EncodingService.Base64Decode, EncodingService, buffer.fromstring(encoded))
		if not ok then
			return false, decoded
		end
		return true, buffer.tostring(decoded)
	elseif typeName == "PhysicalProperties" then
		if raw.customPhysics == false or not raw.density then
			return true, nil
		end
		local okNumbers, values = decodeNumberFields(raw, {
			"density",
			"friction",
			"elasticity",
			"frictionWeight",
			"elasticityWeight",
		})
		if not okNumbers then
			return false, "PhysicalProperties " .. tostring(values)
		end
		if raw.acousticAbsorption ~= nil then
			local acousticAbsorption = tonumber(raw.acousticAbsorption)
			if not acousticAbsorption then
				return false, "PhysicalProperties acousticAbsorption must be a number"
			end
			local okCreate, physicalProperties = pcall(
				PhysicalProperties.new :: any,
				values[1],
				values[2],
				values[3],
				values[4],
				values[5],
				acousticAbsorption
			)
			if not okCreate then
				return false, physicalProperties
			end
			return true, physicalProperties
		end
		return true, PhysicalProperties.new(values[1], values[2], values[3], values[4], values[5])
	elseif typeName == "NumberRange" then
		local okNumbers, values = decodeNumberFields(raw, {
			{ "min", "Min", 1 },
			{ "max", "Max", 2 },
		})
		if not okNumbers then
			return false, "NumberRange " .. tostring(values)
		end
		return true, NumberRange.new(values[1], values[2])
	elseif typeName == "Vector2" then
		local okNumbers, values = decodeNumberFields(raw, { "x", "y" })
		if not okNumbers then
			return false, "Vector2 " .. tostring(values)
		end
		return true, Vector2.new(values[1], values[2])
	elseif typeName == "Vector3" then
		local okNumbers, values = decodeNumberFields(raw, { "x", "y", "z" })
		if not okNumbers then
			return false, "Vector3 " .. tostring(values)
		end
		return true, Vector3.new(values[1], values[2], values[3])
	elseif typeName == "UDim" then
		local okNumbers, values = decodeNumberFields(raw, { "scale", "offset" })
		if not okNumbers then
			return false, "UDim " .. tostring(values)
		end
		return true, UDim.new(values[1], values[2])
	elseif typeName == "UDim2" then
		local okNumbers, values = decodeNumberFields(raw, { "xScale", "xOffset", "yScale", "yOffset" })
		if not okNumbers then
			return false, "UDim2 " .. tostring(values)
		end
		return true, UDim2.new(values[1], values[2], values[3], values[4])
	elseif typeName == "Color3" then
		return decodeColor3(raw)
	elseif typeName == "BrickColor" then
		local number = tonumber(raw.number or raw.BrickColor)
		if not number then
			return false, "BrickColor number must be numeric"
		end
		return true, BrickColor.new(number)
	elseif typeName == "EnumItem" then
		return decodeEnumItem(raw, enumHint)
	elseif typeName == "CFrame" then
		local components = raw.components
		if type(components) ~= "table" or #components ~= 12 then
			return false, "CFrame components must contain 12 numbers"
		end
		local values = table.create(12)
		for i = 1, 12 do
			local component = BridgeValueCodec.decodeNumber(components[i])
			if not component then
				return false, string.format("CFrame component %d must be a number", i)
			end
			values[i] = component
		end
		return true, CFrame.new(table.unpack(values))
	elseif typeName == "Rect" then
		local okNumbers, values = decodeNumberFields(raw, { "minX", "minY", "maxX", "maxY" })
		if not okNumbers then
			return false, "Rect " .. tostring(values)
		end
		return true, Rect.new(values[1], values[2], values[3], values[4])
	elseif typeName == "Font" then
		local ok, font = pcall(function()
			return Font.new(
				tostring(raw.family or ""),
				(Enum.FontWeight :: any)[tostring(raw.weight or "Regular")],
				(Enum.FontStyle :: any)[tostring(raw.style or "Normal")]
			)
		end)
		if ok then
			return true, font
		end
		return false, font
	elseif typeName == "ColorSequence" then
		local keypoints = raw.keypoints
		if type(keypoints) ~= "table" then
			return false, "ColorSequence keypoints must be a table"
		end
		local decoded = table.create(#keypoints)
		for i, keypoint in ipairs(keypoints) do
			if type(keypoint) ~= "table" then
				return false, string.format("ColorSequence keypoint %d must be an object", i)
			end
			local colorRaw = if keypoint.value ~= nil then keypoint.value else keypoint.color or keypoint.Value
			local okColor, color = decodeColor3(colorRaw)
			if not okColor then
				return false, color
			end
			local okNumbers, values = decodeNumberFields(keypoint, { "time" })
			if not okNumbers then
				return false, string.format("ColorSequence keypoint %d %s", i, tostring(values))
			end
			local okKeypoint, decodedKeypoint = pcall(ColorSequenceKeypoint.new, values[1], color)
			if not okKeypoint then
				return false, decodedKeypoint
			end
			decoded[i] = decodedKeypoint
		end
		local okSequence, sequence = pcall(ColorSequence.new, decoded)
		return okSequence, sequence
	elseif typeName == "NumberSequence" then
		local keypoints = raw.keypoints
		if type(keypoints) ~= "table" then
			return false, "NumberSequence keypoints must be a table"
		end
		local decoded = table.create(#keypoints)
		for i, keypoint in ipairs(keypoints) do
			if type(keypoint) ~= "table" then
				return false, string.format("NumberSequence keypoint %d must be an object", i)
			end
			local okNumbers, values = decodeNumberFields(keypoint, { "time", "value", "envelope" })
			if not okNumbers then
				return false, string.format("NumberSequence keypoint %d %s", i, tostring(values))
			end
			local okKeypoint, decodedKeypoint = pcall(NumberSequenceKeypoint.new, values[1], values[2], values[3])
			if not okKeypoint then
				return false, decodedKeypoint
			end
			decoded[i] = decodedKeypoint
		end
		local okSequence, sequence = pcall(NumberSequence.new, decoded)
		return okSequence, sequence
	elseif typeName == "Axes" then
		if type(raw.axes) ~= "table" then
			return false, "Axes axes must be an array"
		end
		local axes = {}
		for _, name in ipairs(raw.axes) do
			local item = (Enum.Axis :: any)[tostring(name)]
			if item == nil then
				return false, "Unknown axis " .. tostring(name)
			end
			axes[#axes + 1] = item
		end
		return true, Axes.new(table.unpack(axes))
	elseif typeName == "Faces" then
		if type(raw.faces) ~= "table" then
			return false, "Faces faces must be an array"
		end
		local faces = {}
		for _, name in ipairs(raw.faces) do
			local item = (Enum.NormalId :: any)[tostring(name)]
			if item == nil then
				return false, "Unknown face " .. tostring(name)
			end
			faces[#faces + 1] = item
		end
		return true, Faces.new(table.unpack(faces))
	elseif typeName == "Ray" then
		local okOrigin, origin = decodeNumberFields(raw.origin, { "x", "y", "z" })
		if not okOrigin then
			return false, "Ray origin " .. tostring(origin)
		end
		local okDirection, direction = decodeNumberFields(raw.direction, { "x", "y", "z" })
		if not okDirection then
			return false, "Ray direction " .. tostring(direction)
		end
		return true,
			Ray.new(Vector3.new(origin[1], origin[2], origin[3]), Vector3.new(direction[1], direction[2], direction[3]))
	elseif typeName == "Ref" then
		return true, decodeRef(raw, context, serviceName)
	end

	return true, raw
end

return BridgeValueCodec
