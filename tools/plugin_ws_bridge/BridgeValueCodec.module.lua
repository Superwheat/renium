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
	if not encodeOk or type(encoded) ~= "string"
		or not string.find(encoded, '"t":"numeric"', 1, true)
		or not string.find(encoded, '"v":"nan"', 1, true)
		or not string.find(encoded, '"v":"inf"', 1, true)
		or not string.find(encoded, '"v":"-inf"', 1, true)
	then
		return false
	end
	local decodeOk, decoded = pcall(HttpService.JSONDecode, HttpService, encoded)
	if not decodeOk
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

return BridgeValueCodec
