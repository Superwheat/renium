local fs = require("@lune/fs")
local luau = require("@lune/luau")

local groups = { { id = 0, name = "Default", mask = -1 } }
local writes = 0
local function find(name)
	for _, group in groups do if group.name == name then return group end end
end
local physics = {
	GetCollisionGroups = function() return table.clone(groups) end,
	GetRegisteredCollisionGroups = function() return table.clone(groups) end,
	IsCollisionGroupRegistered = function(_, name) return find(name) ~= nil end,
	UnregisterCollisionGroup = function(_, name)
		assert(name ~= "Default")
		local removed = find(name)
		table.remove(groups, table.find(groups, removed))
		for _, group in groups do
			local mask = bit32.replace(group.mask, 1, removed.id)
			group.mask = if mask >= 2 ^ 31 then mask - 2 ^ 32 else mask
		end
		writes += 1
	end,
	RegisterCollisionGroup = function(_, name)
		assert(not find(name))
		local id = 0
		while true do
			local used = false
			for _, group in groups do if group.id == id then used = true end end
			if not used then break end
			id += 1
		end
		table.insert(groups, { name = name, id = id, mask = -1 })
		writes += 1
	end,
	CollisionGroupsAreCollidable = function(_, a, b)
		return bit32.band(find(a).mask, bit32.lshift(1, find(b).id)) ~= 0
	end,
	CollisionGroupSetCollidable = function(_, a, b, enabled)
		local left, right = find(a), find(b)
		for _, pair in { { left, right.id }, { right, left.id } } do
			local mask = bit32.replace(pair[1].mask, if enabled then 1 else 0, pair[2])
			pair[1].mask = if mask >= 2 ^ 31 then mask - 2 ^ 32 else mask
		end
		writes += 1
	end,
}
local module = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeCollisionGroups.module.lua"), {
	environment = { game = { GetService = function(_, name) assert(name == "PhysicsService") return physics end } },
})()
-- Exact CollisionGroupData bytes from the user's original RBXL.
local original = "\x01\x05\x00\x04\xfb\xff\xff\xff\x07\x44\x65\x66\x61\x75\x6c\x74\x01\x04\xfb\xff\xff\xff\x10\x53\x74\x75\x64\x69\x6f\x53\x65\x6c\x65\x63\x74\x61\x62\x6c\x65\x02\x04\xfc\xff\xff\xff\x19\x50\x6c\x75\x67\x69\x6e\x5f\x55\x6e\x73\x65\x6c\x65\x63\x74\x61\x62\x6c\x65\x5f\x47\x72\x6f\x75\x70\x03\x04\xe7\xff\xff\xff\x03\x43\x61\x72\x04\x04\xe7\xff\xff\xff\x09\x43\x68\x61\x72\x61\x63\x74\x65\x72"
local default = module.read()
module.write(original)
assert(module.read() == original, "original five-group RBXL bytes changed")
local before = writes
module.write(original)
assert(writes == before, "unchanged collision groups performed writes")
for _, bad in { "", original:sub(1, -2), string.char(2) .. original:sub(2), original .. "x" } do
	assert(not pcall(module.write, bad))
	assert(writes == before, "invalid group data mutated the service")
end
module.write(default)
assert(module.read() == default, "rollback did not restore original group data")
for id = 1, 31 do physics:RegisterCollisionGroup("Group" .. id) end
local maximum = module.read()
assert(#module.decode(maximum) == 32)
module.write(maximum)
assert(module.read() == maximum)
print("Collision groups: original RBXL bytes, exact rollback, no-op writes, malformed data and 32-group limit passed")
