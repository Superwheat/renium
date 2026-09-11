local PhysicsService = game:GetService("PhysicsService")
local CollisionGroups = {}

function CollisionGroups.decode(data: string): { any }
	assert(type(data) == "string" and #data >= 2, "Invalid collision group data")
	local version, count, cursor = string.unpack("<BB", data)
	assert(version == 1 and count >= 1 and count <= 32, "Unsupported collision group data layout")
	local groups, names, ids = {}, {}, {}
	for _ = 1, count do
		local id, maskType, mask, name
		id, maskType, mask, name, cursor = string.unpack("<BBi4s1", data, cursor)
		assert(maskType == 4 and id < 32 and #name > 0 and not names[name] and not ids[id], "Invalid collision group entry")
		names[name], ids[id] = true, name
		groups[#groups + 1] = { id = id, name = name, mask = mask }
	end
	assert(cursor == #data + 1 and ids[0] == "Default", "Invalid collision group data")
	table.sort(groups, function(a, b)
		return a.id < b.id
	end)
	for _, group in ipairs(groups) do
		for _, other in ipairs(groups) do
			assert((bit32.band(group.mask, bit32.lshift(1, other.id)) ~= 0)
				== (bit32.band(other.mask, bit32.lshift(1, group.id)) ~= 0), "Asymmetric collision group masks")
		end
	end
	return groups
end

function CollisionGroups.read(): string
	-- The legacy query is needed for serialized IDs; the replacement omits IDs.
	local groups = PhysicsService:GetCollisionGroups()
	table.sort(groups, function(a, b)
		return a.id < b.id
	end)
	local parts = { string.pack("<BB", 1, #groups) }
	for _, group in ipairs(groups) do
		parts[#parts + 1] = string.pack("<BBi4s1", group.id, 4, group.mask, group.name)
	end
	return table.concat(parts)
end

function CollisionGroups.write(data: string)
	local groups = CollisionGroups.decode(data)
	local desired = {}
	for _, group in ipairs(groups) do
		desired[group.name] = true
	end
	for _, group in ipairs(PhysicsService:GetRegisteredCollisionGroups()) do
		if not desired[group.name] then
			PhysicsService:UnregisterCollisionGroup(group.name)
		end
	end
	for _, group in ipairs(groups) do
		if not PhysicsService:IsCollisionGroupRegistered(group.name) then
			PhysicsService:RegisterCollisionGroup(group.name)
		end
	end
	for index, group in ipairs(groups) do
		for otherIndex = index, #groups do
			local other = groups[otherIndex]
			local collidable = bit32.band(group.mask, bit32.lshift(1, other.id)) ~= 0
			if PhysicsService:CollisionGroupsAreCollidable(group.name, other.name) ~= collidable then
				PhysicsService:CollisionGroupSetCollidable(group.name, other.name, collidable)
			end
		end
	end
end

return CollisionGroups
