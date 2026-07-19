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
	local out = table.create(values.n)
	for index = 1, values.n do
		local encoded = BridgeValueCodec.encodeNumber(values[index])
		if not encoded then
			error("Numeric component encoder received a non-number")
		end
		out[index] = encoded
	end
	return out
end

return BridgeValueCodec
