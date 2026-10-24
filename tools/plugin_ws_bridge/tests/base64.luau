-- Lune 0.8 has no base64 API. Keep this fixture codec shared with embedded tests.
local alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
local codec = {}

function codec.encode(input)
	local output = {}
	for index = 1, #input, 3 do
		local a, b, c = string.byte(input, index, index + 2)
		local value = a * 65536 + (b or 0) * 256 + (c or 0)
		for shift = 18, 0, -6 do
			local digit = bit32.band(bit32.rshift(value, shift), 63) + 1
			table.insert(output, if shift == 6 and not b or shift == 0 and not c then "=" else alphabet:sub(digit, digit))
		end
	end
	return table.concat(output)
end

function codec.decode(input)
	local output = {}
	for index = 1, #input, 4 do
		local value = 0
		for offset = 0, 3 do
			local digit = input:sub(index + offset, index + offset)
			value = value * 64 + (if digit == "=" then 0 else assert(alphabet:find(digit, 1, true)) - 1)
		end
		table.insert(output, string.char(bit32.rshift(value, 16)))
		if input:sub(index + 2, index + 2) ~= "=" then
			table.insert(output, string.char(bit32.band(bit32.rshift(value, 8), 255)))
		end
		if input:sub(index + 3, index + 3) ~= "=" then
			table.insert(output, string.char(bit32.band(value, 255)))
		end
	end
	return table.concat(output)
end

return codec
