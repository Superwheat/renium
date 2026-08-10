local CollectionService = game:GetService("CollectionService")
local ScriptEditorService = game:GetService("ScriptEditorService")

local Error = require(script.Parent.Error)

local TERRAIN_MATERIAL_COLORS = {
	Enum.Material.Grass,
	Enum.Material.Slate,
	Enum.Material.Concrete,
	Enum.Material.Brick,
	Enum.Material.Sand,
	Enum.Material.WoodPlanks,
	Enum.Material.Rock,
	Enum.Material.Glacier,
	Enum.Material.Snow,
	Enum.Material.Sandstone,
	Enum.Material.Mud,
	Enum.Material.Basalt,
	Enum.Material.Ground,
	Enum.Material.CrackedLava,
	Enum.Material.Asphalt,
	Enum.Material.Cobblestone,
	Enum.Material.Ice,
	Enum.Material.LeafyGrass,
	Enum.Material.Salt,
	Enum.Material.Limestone,
	Enum.Material.Pavement,
}

local function isAttributeNameValid(attributeName)

	return #attributeName <= 100

		and attributeName:match("[^%w%.%-_/]") == nil
end

local function isAttributeNameReserved(attributeName)

	return attributeName:sub(1, 3) == "RBX"
end

return {
	Instance = {
		Attributes = {
			read = function(instance)
				return true, instance:GetAttributes()
			end,
			write = function(instance, _, value)
				if typeof(value) ~= "table" then
					return false, Error.new(Error.Kind.CannotParseBinaryString)
				end

				local existing = instance:GetAttributes()
				local didAllWritesSucceed = true

				for attributeName, attributeValue in pairs(value) do
					if isAttributeNameReserved(attributeName) then

						continue
					end

					if not isAttributeNameValid(attributeName) then
						didAllWritesSucceed = false
						continue
					end

					instance:SetAttribute(attributeName, attributeValue)
				end

				for existingAttributeName in pairs(existing) do
					if isAttributeNameReserved(existingAttributeName) then
						continue
					end

					if not isAttributeNameValid(existingAttributeName) then
						didAllWritesSucceed = false
						continue
					end

					if value[existingAttributeName] == nil then
						instance:SetAttribute(existingAttributeName, nil)
					end
				end

				return didAllWritesSucceed
			end,
		},
		Tags = {
			read = function(instance)
				return true, CollectionService:GetTags(instance)
			end,
			write = function(instance, _, value)
				local existingTags = CollectionService:GetTags(instance)

				local unseenTags = {}
				for _, tag in ipairs(existingTags) do
					unseenTags[tag] = true
				end

				for _, tag in ipairs(value) do
					unseenTags[tag] = nil
					CollectionService:AddTag(instance, tag)
				end

				for tag in pairs(unseenTags) do
					CollectionService:RemoveTag(instance, tag)
				end

				return true
			end,
		},
	},
	LocalizationTable = {
		Contents = {
			read = function(instance, _)
				return true, instance:GetContents()
			end,
			write = function(instance, _, value)
				instance:SetContents(value)
				return true
			end,
		},
	},
	Model = {
		Scale = {
			read = function(instance, _, _)
				return true, instance:GetScale()
			end,
			write = function(instance, _, value)
				return true, instance:ScaleTo(value)
			end,
		},
		WorldPivotData = {
			read = function(instance)
				return true, instance:GetPivot()
			end,
			write = function(instance, _, value)
				if value == nil then
					return true, nil
				else
					instance.WorldPivot = value
					return true
				end
			end,
		},
		WorldPivot = {
			read = function(instance)
				return true, instance:GetPivot()
			end,
			write = function(instance, _, value)
				instance.WorldPivot = value
				return true
			end,
		},
		Origin = {
			read = function(instance)
				return true, instance:GetPivot()
			end,
			write = function(instance, _, value)
				instance:PivotTo(value)
				return true
			end,
		},
	},
	Terrain = {
		MaterialColors = {
			read = function(instance: Terrain)

				local colors = {}
				for _, material in TERRAIN_MATERIAL_COLORS do
					colors[material] = instance:GetMaterialColor(material)
				end

				return true, colors
			end,
			write = function(instance: Terrain, _, value: { [Enum.Material]: Color3 })
				if typeof(value) ~= "table" then
					return false, Error.new(Error.Kind.CannotParseBinaryString)
				end

				for material, color in value do
					instance:SetMaterialColor(material, color)
				end

				return true
			end,
		},
	},
	Script = {
		Source = {
			read = function(instance: Script)
				return true, ScriptEditorService:GetEditorSource(instance)
			end,
			write = function(instance: Script, _, value: string)
				task.spawn(function()
					ScriptEditorService:UpdateSourceAsync(instance, function()
						return value
					end)
				end)
				return true
			end,
		},
	},
	ModuleScript = {
		Source = {
			read = function(instance: ModuleScript)
				return true, ScriptEditorService:GetEditorSource(instance)
			end,
			write = function(instance: ModuleScript, _, value: string)
				task.spawn(function()
					ScriptEditorService:UpdateSourceAsync(instance, function()
						return value
					end)
				end)
				return true
			end,
		},
	},
	StyleRule = {
		PropertiesSerialize = {
			read = function(instance: StyleRule)
				return true, instance:GetProperties()
			end,
			write = function(instance: StyleRule, _, value: { [any]: any })
				if typeof(value) ~= "table" then
					return false, Error.new(Error.Kind.CannotParseBinaryString)
				end

				local existing = instance:GetProperties()

				for itemName, itemValue in pairs(value) do
					instance:SetProperty(itemName, itemValue)
				end

				for existingItemName in pairs(existing) do
					if value[existingItemName] == nil then
						instance:SetProperty(existingItemName, nil)
					end
				end

				return true
			end,
		},
	},
}
