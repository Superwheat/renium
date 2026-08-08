local MaterialService = game:GetService("MaterialService")

local materialByProperty = {}
for _, material in Enum.Material:GetEnumItems() do
	materialByProperty[material.Name .. "Name"] = material
end

local BridgeMaterialService = {}

function BridgeMaterialService.readOverride(instance: Instance, propertyName: string): (boolean, any)
	if instance ~= MaterialService then
		return false, nil
	end
	local material = materialByProperty[propertyName]
	if material == nil then
		return false, nil
	end
	return true, MaterialService:GetBaseMaterialOverride(material)
end

function BridgeMaterialService.writeOverride(instance: Instance, propertyName: string, value: any): (boolean, any)
	if instance ~= MaterialService then
		return false, nil
	end
	local material = materialByProperty[propertyName]
	if material == nil then
		return false, nil
	end
	return pcall(MaterialService.SetBaseMaterialOverride, MaterialService, material, tostring(value))
end

return BridgeMaterialService
