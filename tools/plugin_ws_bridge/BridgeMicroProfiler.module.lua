-- Fixed LibMP v1 control operations, not a caller-supplied command tunnel.
-- Validate the engine protocol and every acknowledgement before accepting it.
local BridgeMicroProfiler = {}

function BridgeMicroProfiler.create(getService, finishFrame)
	local serial = 0
	local active = false
	local copying = false
	local function control(command, value)
		local service = getService()
		local response = buffer.create(4096)
		local size = service:ProcessCommand(response, 0, 0, response, 0, 4096)
		assert(size == 20 and buffer.readu32(response, 0) == 65536
			and buffer.readu32(response, 4) == 65536 and buffer.readu32(response, 8) == 65536
			and buffer.readu32(response, 12) == 65536, "MicroProfiler control protocol changed; update Renium")
		serial = (serial + 1) % 4294967296
		local width = if command == 40 then 4 else 1
		local request = buffer.create(32 + width)
		buffer.writeu32(request, 4, 0x46423041)
		buffer.writeu16(request, 8, command)
		buffer.writeu16(request, 10, 1)
		buffer.writeu32(request, 12, serial)
		buffer.writeu16(request, 16, 0x0401)
		buffer.writeu32(request, 20, 1)
		buffer.writeu32(request, 24, width)
		buffer.writeu32(request, 28, width)
		if width == 4 then
			buffer.writeu32(request, 32, value)
		else
			buffer.writeu8(request, 32, value)
		end
		size = service:ProcessCommand(request, 0, buffer.len(request), response, 0, 4096)
		assert(size == 33 and buffer.readu32(response, 0) == 0
			and buffer.readu32(response, 4) == 0x46423041 and buffer.readu16(response, 8) == command
			and buffer.readu16(response, 10) == 1 and buffer.readu32(response, 12) == serial
			and buffer.readu32(response, 16) == 0x00010401 and buffer.readu32(response, 20) == 1
			and buffer.readu32(response, 24) == 1 and buffer.readu32(response, 28) == 1
			and buffer.readu8(response, 32) == 1, "Studio rejected MicroProfiler control")
	end

	local function disable(capturePaused)
		-- Either control can fail independently; still attempt the other one.
		local stopped, stopError = true, nil
		if not capturePaused then
			stopped, stopError = pcall(control, 20, 0)
		end
		local disabled, disableError = pcall(control, 10, 0)
		active = not (stopped and disabled)
		assert(stopped, stopError)
		assert(disabled, disableError)
	end

	local function capture(snapshot, finish)
		assert(not copying, "A MicroProfiler snapshot is already being copied")
		copying = true
		local paused = false
		local ok, result = pcall(function()
			if snapshot then
				-- Both size and range reads synchronize the native dump. Refresh
				-- while collecting, then freeze it before measuring/copying bytes.
				getService():GetDataSize(0)
				control(20, 0)
				paused = true
				-- The acknowledgement changes collection state, but the current
				-- profiler frame still has to finish publishing its descriptors.
				finishFrame()
				assert(active, "MicroProfiler capture ended while copying")
				return snapshot()
			end
			return nil
		end)
		local cleaned, cleanupError = true, nil
		if active and finish then
			cleaned, cleanupError = pcall(disable, paused)
		elseif active and paused then
			cleaned, cleanupError = pcall(control, 20, 1)
		end
		copying = false
		assert(cleaned, if ok then cleanupError else `{result}; cleanup failed: {cleanupError}`)
		assert(ok, result)
		return result
	end

	return {
		start = function(frames)
			assert(type(frames) == "number" and frames % 1 == 0 and frames >= 1 and frames <= 256, "MicroProfiler frame limit must be 1–256")
			assert(not active and not copying, "This runtime already has an active or completing MicroProfiler capture")
			control(40, frames)
			active = true
			local ok, result = pcall(function()
				control(10, 1)
				control(20, 1)
				-- Prime only after enabling, using this instrumentation generation.
				getService():GetDataSize(0)
			end)
			if not ok then
				local cleaned, cleanupError = pcall(disable)
				error(if cleaned then result else `{result}; cleanup failed: {cleanupError}`, 0)
			end
		end,
		stop = function(snapshot)
			assert(active, "This runtime has no active Renium MicroProfiler capture")
			return capture(snapshot, true)
		end,
		read = function(snapshot)
			-- Do not pause or resume a capture this runtime did not start.
			return if active then capture(snapshot, false) else snapshot()
		end,
		cleanup = function()
			if active then
				disable()
			end
		end,
	}
end

return BridgeMicroProfiler
