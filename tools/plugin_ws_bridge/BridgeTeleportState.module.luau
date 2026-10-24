local State = {}

function State.create(now)
	local self = { phase = "off", id = 0, committedId = nil, pending = nil, touchedAt = now() }
	function self.arm()
		assert(self.phase == "off" or self.phase == "idle", "Teleport is busy; finish or stop the current attempt")
		self.phase = "idle"
	end
	function self.observe(place, player, data, reserved, failure)
		if self.phase ~= "idle" and self.phase ~= "collecting" then
			return
		end
		if self.phase == "idle" then
			self.id += 1
			self.pending = { id = self.id, placeId = place, players = {}, data = {}, reserved = false }
			self.phase = "collecting"
		end
		local pending = self.pending
		if pending.placeId ~= place then
			pending.error = "Players requested different destinations"
		end
		if type(place) ~= "number" or place <= 0 or place % 1 ~= 0 or place > 9007199254740991 then
			pending.error = "Invalid destination ID"
		end
		if pending.data[player] == nil then
			table.insert(pending.players, player)
		end
		pending.data[player] = data
		pending.reserved = pending.reserved or reserved
		pending.error = pending.error or failure
		self.touchedAt = now()
	end
	function self.poll()
		if self.phase == "collecting" and now() - self.touchedAt >= 0.3 then
			self.phase = "loading"
		end
		return if self.phase == "loading" then self.pending else nil
	end
	function self.expect(id, phase)
		assert(id == self.id and self.phase == phase, "Teleport attempt expired or changed phase")
		self.touchedAt = now()
	end
	function self.advance(id, before, after)
		self.expect(id, before)
		if after == "committing" then
			self.committedId = id
		end
		self.phase = after
	end
	function self.fail(id)
		if id ~= self.id then
			return false
		end
		local stopRequired = self.committedId == id or self.phase == "failed"
		self.phase = if stopRequired then "failed" else "idle"
		self.pending = nil
		return stopRequired
	end
	function self.complete(id)
		self.advance(id, "arriving", "idle")
		self.pending = nil
	end
	return self
end

return State
