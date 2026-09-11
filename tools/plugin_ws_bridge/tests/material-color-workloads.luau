local fs = require("@lune/fs")
local luau = require("@lune/luau")
local roblox = require("@lune/roblox")

local materials = {}
for _, material in roblox.Enum.Material:GetEnumItems() do
	materials[material.Name] = material
end
local environment = {
	game = { GetService = function() return {} end },
	Enum = { Material = materials },
	Color3 = roblox.Color3,
	script = { Parent = { Error = {} } },
	require = function(value) return value end,
}
local codec = luau.load(fs.readFile("tools/plugin_ws_bridge/BridgeValueCodec.module.lua"), { environment = environment })()
local custom = luau.load(fs.readFile("tools/plugin_ws_bridge/rbx_dom_lua/customProperties.module.lua"), { environment = environment })()
local ok, palette = codec.decode({ _type = "MaterialColors", colors = { Grass = { 12, 34, 56 }, Rock = { 255, 0, 128 } } })
assert(ok, tostring(palette))
assert(palette[materials.Grass] == roblox.Color3.fromRGB(12, 34, 56))
local saved = {}
local terrain = {
	SetMaterialColor = function(_, material, color) saved[material] = color end,
	GetMaterialColor = function(_, material) return saved[material] or roblox.Color3.new(0, 0, 0) end,
}
assert(custom.Terrain.MaterialColors.write(terrain, "MaterialColors", palette))
local readOk, read = custom.Terrain.MaterialColors.read(terrain)
assert(readOk and read[materials.Grass] == palette[materials.Grass])
assert(read[materials.Rock] == palette[materials.Rock])
for _, invalid in {
	{ base64 = "AA==" },
	{ colors = { MissingMaterial = { 1, 2, 3 } } },
	{ colors = { Grass = { 1, 2 } } },
	{ colors = { Grass = { 1, 2, 256 } } },
	{ colors = { Grass = { 1, 2, 0 / 0 } } },
} do
	invalid._type = "MaterialColors"
	assert(not codec.decode(invalid), "invalid palette was accepted")
end
print("Terrain color transport: exact RGB decoding, existing custom setter/readback and invalid palettes passed")
