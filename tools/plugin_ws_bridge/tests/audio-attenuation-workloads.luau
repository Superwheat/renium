local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")

local environment = {
	game = { GetService = function() return {} end },
	Enum = { Material = setmetatable({}, { __index = function() return {} end }) },
	script = { Parent = { Error = {}, customProperties = {} } },
	require = function(value)
		if value.Kind == nil then
			value.Kind = { CannotParseBinaryString = "CannotParseBinaryString" }
			value.new = function(kind) return kind end
		end
		return value
	end,
}
local custom = luau.load(fs.readFile("tools/plugin_ws_bridge/rbx_dom_lua/customProperties.module.luau"), { environment = environment })()
environment.script.Parent.customProperties = custom
local descriptor = luau.load(fs.readFile("tools/plugin_ws_bridge/rbx_dom_lua/PropertyDescriptor.module.luau"), { environment = environment })()

local function pack(input, gain)
	return string.pack("<ff", input, gain)
end
local curve = "\0" .. pack(10, 1) .. pack(110, 0.09) .. pack(210, 0.0467)
local saved = {}
local emitter = {
	GetDistanceAttenuation = function() return saved.distance or {} end,
	SetDistanceAttenuation = function(_, points) saved.distance = points end,
	GetAngleAttenuation = function() return saved.angle or {} end,
	SetAngleAttenuation = function(_, points) saved.angle = points end,
}

local distance = custom.AudioEmitter.DistanceAttenuation
assert(distance.write(emitter, "DistanceAttenuation", curve))
local _, expected = string.unpack("<ff", pack(110, 0.09))
assert(saved.distance[10] == 1 and saved.distance[110] == expected and saved.distance[210] ~= nil, "Curve points must decode as float32 pairs")
local count = 0
for _ in pairs(saved.distance) do count += 1 end
assert(count == 3)
local readOk, read = distance.read(emitter)
assert(readOk and read == curve, "Reading a curve must reproduce the saved bytes")

saved.distance = { [110] = 0.5, [10] = 1 }
assert(select(2, distance.read(emitter)) == "\0" .. pack(10, 1) .. pack(110, 0.5), "Points must be written sorted by input")
saved.distance = {}
assert(select(2, distance.read(emitter)) == "\0", "An empty curve is the one-byte default")
assert(distance.write(emitter, "DistanceAttenuation", "\0"))
assert(next(saved.distance) == nil)
assert(distance.write(emitter, "DistanceAttenuation", ""))
assert(next(saved.distance) == nil)

local angle = custom.AudioEmitter.AngleAttenuation
assert(angle.write(emitter, "AngleAttenuation", "\0" .. pack(0, 1) .. pack(90, 0.25)))
assert(saved.angle[90] == 0.25)
assert(select(2, angle.read(emitter)) == "\0" .. pack(0, 1) .. pack(90, 0.25))

for _, invalid in { "\1" .. pack(10, 1), "\0abc", 42, nil } do
	saved.distance = { [10] = 1 }
	local ok = distance.write(emitter, "DistanceAttenuation", invalid)
	assert(not ok, "Malformed curves must be rejected")
	assert(saved.distance[10] == 1, "A rejected curve must not touch the emitter")
end

local raw = { DataType = { Value = "BinaryString" }, Scriptability = "None", Kind = { Canonical = { Serialization = "Serializes" } } }
assert(descriptor.fromRaw(raw, "AudioEmitter", "DistanceAttenuation").scriptability == "Custom", "Custom accessors override an unscriptable database entry")
assert(descriptor.fromRaw(raw, "AudioEmitter", "AudioInteractionGroup").scriptability == "None")
assert(descriptor.fromRaw(raw, "Part", "DistanceAttenuation").scriptability == "None")
local okRead, value = descriptor.fromRaw(raw, "AudioEmitter", "AngleAttenuation"):read(emitter)
assert(okRead and value == "\0" .. pack(0, 1) .. pack(90, 0.25), "Descriptor reads route through the custom accessor")
