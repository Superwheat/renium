local HttpService = game:GetService("HttpService")

local BridgeChunking = {}
local MAX_CHUNK_BYTES = 8 * 1024 * 1024

function BridgeChunking.jsonEncodeTimed(value)
	local encodeStarted = os.clock()
	local encoded = HttpService:JSONEncode(value)
	return encoded, (os.clock() - encodeStarted) * 1000
end

function BridgeChunking.chunkEncodedString(encoded, startIndex, maxLen, encodeMs)
	local total = #encoded
	local requestedStart = tonumber(startIndex) or 1
	local requestedLen = tonumber(maxLen) or 2000
	local startPos = math.max(1, math.floor(requestedStart))
	local take = math.clamp(math.floor(requestedLen), 1, MAX_CHUNK_BYTES)
	if startPos > total then
		return {
			start = startPos,
			nextStart = startPos,
			total = total,
			chunk = "",
			pluginEncodeMs = math.max(0, encodeMs or 0),
		}
	end
	local finish = math.min(total, startPos + take - 1)
	return {
		start = startPos,
		nextStart = finish + 1,
		total = total,
		chunk = string.sub(encoded, startPos, finish),
		pluginEncodeMs = math.max(0, encodeMs or 0),
	}
end

function BridgeChunking.getCompactInstanceBatchCacheKey(startIndex, maxCount)
	return "compact:" .. tostring(startIndex or 1) .. ":" .. tostring(maxCount or 300)
end

function BridgeChunking.getSourceBatchCacheKey(instancePaths: { string })
	return "paths:" .. table.concat(instancePaths, "\\0")
end

function BridgeChunking.getSourceRangeBatchCacheKey(startIndex, maxCount)
	return "range:" .. tostring(math.max(1, startIndex or 1)) .. ":" .. tostring(math.max(1, maxCount or 64))
end

return BridgeChunking
