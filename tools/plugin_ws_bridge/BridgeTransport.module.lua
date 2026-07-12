local HttpService = game:GetService("HttpService")

local BridgeTransport = {}
local MAX_RESPONSE_BYTES = 16 * 1024 * 1024
local MAX_RAW_CHUNK_BYTES = 8 * 1024 * 1024

local RAW_CHUNK_METHODS = {
	getInstanceBatchChunk = true,
	getInstanceBatchCompactChunk = true,
	getClassDefaultsChunk = true,
	getScriptPathsChunk = true,
	getSourceBatchChunk = true,
	getSourceRangeBatchCompactChunk = true,
	getSourceChunk = true,
}

function BridgeTransport.sendEnvelope(client, envelope)
	local ok, encoded = pcall(function()
		return HttpService:JSONEncode(envelope)
	end)
	if not ok then
		return false, "Failed to encode bridge response"
	end
	if #encoded > MAX_RESPONSE_BYTES then
		return false, "Bridge response exceeds safe size limit"
	end
	local sent, sendErr = pcall(function()
		client:Send(encoded)
	end)
	return sent, sendErr
end

local function isRawChunkResult(method, result)
	return RAW_CHUNK_METHODS[method] == true
		and type(result) == "table"
		and type(result.chunk) == "string"
		and type(result.nextStart) == "number"
		and type(result.total) == "number"
end

local function sendRawChunkResponse(client, id, result, serverMs)
	local startValue = math.max(1, tonumber(result.start) or 1)
	local nextStartValue = math.max(startValue, tonumber(result.nextStart) or startValue)
	local totalValue = math.max(0, tonumber(result.total) or 0)
	local encodeMs = math.max(0, tonumber(result.pluginEncodeMs) or 0)
	local header = ("RBS1 %s %d %d %d %.3f %.3f"):format(
		tostring(id),
		startValue,
		nextStartValue,
		totalValue,
		serverMs,
		encodeMs
	)
	local payload = type(result.chunk) == "string" and result.chunk or ""
	if #payload > MAX_RAW_CHUNK_BYTES then
		return false, ("Bridge chunk exceeds safe size limit (%d bytes; maximum is %d)"):format(#payload, MAX_RAW_CHUNK_BYTES)
	end
	local sent, sendErr = pcall(function()
		client:Send(header .. "\n" .. payload)
	end)
	return sent, sendErr
end

function BridgeTransport.sendSuccessResponse(channelId, client, id, method, result, serverMs)
	if isRawChunkResult(method, result) then
		return sendRawChunkResponse(client, id, result, serverMs)
	end
	return BridgeTransport.sendEnvelope(client, {
		id = id,
		ok = true,
		result = result,
		channel = channelId,
		timings = {
			serverMs = serverMs,
		},
	})
end

return BridgeTransport
