local BridgeTransactionUpload = {}

local SESSION_TTL_SECONDS = 120
local MAX_SESSIONS = 4
local MAX_CHUNKS = 4096
local MAX_ROWS = 1000000

local function denseArrayLength(value: any): number?
	if type(value) ~= "table" then
		return nil
	end
	local count = 0
	for key in pairs(value) do
		if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then
			return nil
		end
		count += 1
	end
	return if count == #value then count else nil
end

function BridgeTransactionUpload.create(beginTransaction, transactionBegan)
	local sessions = {}

	local function removeExpired()
		local now = os.clock()
		for id, session in pairs(sessions) do
			if now - session.updatedAt > SESSION_TTL_SECONDS then
				sessions[id] = nil
			end
		end
	end

	local function armExpiry(id: string, session)
		session.updatedAt = os.clock()
		if session.expiryArmed then
			return
		end
		session.expiryArmed = true
		local function expireWhenIdle()
			if sessions[id] ~= session then
				return
			end
			local remaining = SESSION_TTL_SECONDS - (os.clock() - session.updatedAt)
			if remaining > 0 then
				task.delay(remaining, expireWhenIdle)
			else
				sessions[id] = nil
			end
		end
		task.delay(SESSION_TTL_SECONDS, expireWhenIdle)
	end

	local api = {}

	function api.begin(params)
		removeExpired()
		local id = tostring(params.transactionId or "")
		local totalChunks = tonumber(params.totalChunks)
		local rowCount = tonumber(params.rowCount)
		if
			id == ""
			or sessions[id] ~= nil
			or not totalChunks
			or totalChunks < 1
			or totalChunks > MAX_CHUNKS
			or totalChunks % 1 ~= 0
			or not rowCount
			or rowCount < 1
			or rowCount > MAX_ROWS
			or rowCount % 1 ~= 0
		then
			error("Invalid editor transaction upload")
		end
		local count = 0
		for _ in pairs(sessions) do
			count += 1
		end
		if count >= MAX_SESSIONS then
			error("Too many editor transaction uploads")
		end
		local session = {
			transactionId = id,
			services = params.services,
			hasInstanceChanges = params.hasInstanceChanges == true,
			destructiveServices = params.destructiveServices,
			nativeImport = params.nativeImport == true,
			nativeImportServices = params.nativeImportServices,
			mutationRoots = params.mutationRoots,
			totalChunks = totalChunks,
			rowCount = rowCount,
			chunks = table.create(totalChunks),
			receivedChunks = 0,
			receivedRows = 0,
		}
		sessions[id] = session
		armExpiry(id, session)
		return { ok = true, transactionId = id }
	end

	function api.append(params)
		removeExpired()
		local id = tostring(params.transactionId or "")
		local session = sessions[id]
		local index = tonumber(params.index)
		local rowCount = denseArrayLength(params.rows)
		if
			type(session) ~= "table"
			or not index
			or index < 1
			or index > session.totalChunks
			or index % 1 ~= 0
			or not rowCount
		then
			error("Invalid editor transaction upload chunk")
		end
		if session.chunks[index] == nil then
			if session.receivedRows + rowCount > session.rowCount then
				error("Editor transaction upload exceeds its declared row count")
			end
			for _, row in ipairs(params.rows) do
				if
					type(row) ~= "table"
					or type(row.change) ~= "table"
					or (row.kind ~= "source" and row.kind ~= "property" and row.kind ~= "postCommitProperty")
				then
					error("Invalid editor transaction upload row")
				end
			end
			session.chunks[index] = params.rows
			session.receivedChunks += 1
			session.receivedRows += rowCount
		end
		armExpiry(id, session)
		return { ok = true, rows = rowCount }
	end

	function api.finish(params)
		removeExpired()
		local id = tostring(params.transactionId or "")
		local session = sessions[id]
		sessions[id] = nil
		if
			type(session) ~= "table"
			or session.receivedChunks ~= session.totalChunks
			or session.receivedRows ~= session.rowCount
		then
			error("Editor transaction upload is incomplete")
		end
		local transaction = {
			transactionId = session.transactionId,
			services = session.services,
			hasInstanceChanges = session.hasInstanceChanges,
			destructiveServices = session.destructiveServices,
			nativeImport = session.nativeImport,
			nativeImportServices = session.nativeImportServices,
			mutationRoots = session.mutationRoots,
			sourceChanges = {},
			propertyChanges = {},
			postCommitPropertyChanges = {},
		}
		for index = 1, session.totalChunks do
			for _, row in ipairs(session.chunks[index]) do
				if row.kind == "source" then
					transaction.sourceChanges[#transaction.sourceChanges + 1] = row.change
				elseif row.kind == "property" then
					transaction.propertyChanges[#transaction.propertyChanges + 1] = row.change
				else
					transaction.postCommitPropertyChanges[#transaction.postCommitPropertyChanges + 1] = row.change
				end
			end
		end
		local result = beginTransaction(transaction)
		transactionBegan(id, transaction)
		return result
	end

	function api.cancel(params)
		local id = tostring(params.transactionId or "")
		local found = sessions[id] ~= nil
		sessions[id] = nil
		return { ok = true, found = found }
	end

	return api
end

return BridgeTransactionUpload
