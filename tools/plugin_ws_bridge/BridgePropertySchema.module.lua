local BridgePropertySchema = {}

local COMPACT_TYPE_KEY_BY_RBX_DOM_VALUE_TYPE = {
	Bool = "Bool",
	Int32 = "Number",
	Int64 = "Number",
	Float32 = "Number",
	Float64 = "Number",
	String = "String",
	BinaryString = "BinaryString",
	ContentId = "ContentId",
	Ref = "Ref",
	Vector2 = "Vector2",
	Vector3 = "Vector3",
	UDim = "UDim",
	UDim2 = "UDim2",
	Color3 = "Color3",
	Color3uint8 = "Color3",
	ColorSequence = "ColorSequence",
	NumberRange = "NumberRange",
	NumberSequence = "NumberSequence",
	PhysicalProperties = "PhysicalProperties",
	CFrame = "CFrame",
	OptionalCFrame = "CFrame",
	Rect = "Rect",
	Font = "Font",
	BrickColor = "BrickColor",
	Axes = "Axes",
	Faces = "Faces",
	Ray = "Ray",
}

local VALUE_INSTANCE_VALUE_TYPES = {
	BinaryStringValue = "BinaryString",
	BoolValue = "Bool",
	BrickColorValue = "BrickColor",
	CFrameValue = "CFrame",
	Color3Value = "Color3",
	DoubleConstrainedValue = "Float64",
	IntConstrainedValue = "Int64",
	IntValue = "Int64",
	NumberValue = "Float64",
	ObjectValue = "Ref",
	StringValue = "String",
	Vector3Value = "Vector3",
}

local TRIANGLE_MESH_PART_CLASS = "TriangleMeshPart"
local MESH_SIZE_TRANSPORT_PROPERTY = "MeshSize"

local rbxDomValueTypeInfo
local rbxDomPropertyTypeInfo
local schemaPropertyNameForClass

local function isSerializingAliasCanonicalProperty(propertyData)
	local kind = propertyData and propertyData.Kind
	local canonical = type(kind) == "table" and kind.Canonical or nil
	local serialization = type(canonical) == "table" and canonical.Serialization or nil
	return type(serialization) == "table" and type(serialization.SerializesAs) == "string" and serialization.SerializesAs ~= ""
end

local function hasBlockedStudioPropertyTag(propertyData)
	local tags = propertyData and propertyData.Tags
	if type(tags) ~= "table" then
		return false
	end
	for _, tag in ipairs(tags) do
		if tag == "Hidden" or tag == "Deprecated" or tag == "NotBrowsable" or tag == "WriteOnly" then
			return true
		end
		if tag == "ReadOnly" and not isSerializingAliasCanonicalProperty(propertyData) then
			return true
		end
	end
	return false
end

local function isRbxDomPropertySerializable(propertyData)
	local kind = propertyData and propertyData.Kind
	if type(kind) ~= "table" then
		return false
	end

	if type(kind.Alias) == "table" then
		return false
	end

	local canonical = kind.Canonical
	if type(canonical) ~= "table" then
		return false
	end

	local serialization = canonical.Serialization
	if serialization == nil then
		return true
	end
	if type(serialization) == "string" then
		return serialization ~= "DoesNotSerialize"
	end
	return true
end

local function isStudioEditableProperty(propertyData)
	return not hasBlockedStudioPropertyTag(propertyData) and isRbxDomPropertySerializable(propertyData)
end

local function rbxDomPropertyValueType(propertyData)
	local dataType = propertyData and propertyData.DataType
	if type(dataType) ~= "table" then
		return nil
	end
	return dataType.Value
end

local function classHasTag(classes, className, tagName)
	local classData = classes[className]
	local tags = type(classData) == "table" and classData.Tags
	if type(tags) ~= "table" then
		return false
	end
	for _, tag in ipairs(tags) do
		if tag == tagName then
			return true
		end
	end
	return false
end

local function classIsA(classes, className, ancestorClassName)
	local current = className
	local seen = {}
	while type(current) == "string" and current ~= "" and not seen[current] do
		if current == ancestorClassName then
			return true
		end
		seen[current] = true
		local classData = classes[current]
		current = type(classData) == "table" and classData.Superclass or nil
	end
	return false
end

local function propertyDataForClass(classes, className, propertyName)
	local current = className
	local seen = {}
	while type(current) == "string" and current ~= "" and not seen[current] do
		seen[current] = true
		local classData = classes[current]
		local properties = type(classData) == "table" and classData.Properties
		if type(properties) == "table" and properties[propertyName] ~= nil then
			return properties[propertyName]
		end
		current = type(classData) == "table" and classData.Superclass or nil
	end
	return nil
end

local function addSupplementalReadableTransportProperties(classes, className, names, seen, compactTypeIds)
	if not classIsA(classes, className, TRIANGLE_MESH_PART_CLASS) then
		return
	end

	local propertyData = propertyDataForClass(classes, className, MESH_SIZE_TRANSPORT_PROPERTY)
	if rbxDomPropertyValueType(propertyData) ~= "Vector3" then
		return
	end

	local typeId, enumType = rbxDomPropertyTypeInfo(propertyData, compactTypeIds)
	if typeId == nil then
		return
	end

	local schemaName = schemaPropertyNameForClass(className, MESH_SIZE_TRANSPORT_PROPERTY)
	local schemaKey = string.lower(schemaName)
	if not seen[schemaKey] then
		seen[schemaKey] = true
		names[#names + 1] = { schemaName, typeId, enumType or false }
	end
end

local function isEngineManagedStudioProperty(classes, className, propertyData)
	local valueType = rbxDomPropertyValueType(propertyData)
	if valueType == "UniqueId" or valueType == "SecurityCapabilities" then
		return true
	end

	return valueType == "Ref" and classHasTag(classes, className, "Service")
end

local function isRbxDomPropertyTypeSupported(propertyData)
	local dataType = propertyData and propertyData.DataType
	if type(dataType) ~= "table" then
		return false
	end
	if type(dataType.Enum) == "string" and dataType.Enum ~= "" then
		return true
	end
	local valueType = dataType.Value
	return type(valueType) == "string" and COMPACT_TYPE_KEY_BY_RBX_DOM_VALUE_TYPE[valueType] ~= nil
end

rbxDomPropertyTypeInfo = function(propertyData, compactTypeIds)
	local dataType = propertyData and propertyData.DataType
	if type(dataType) ~= "table" then
		return nil, nil
	end

	local enumType = dataType.Enum
	if type(enumType) == "string" and enumType ~= "" then
		return compactTypeIds.EnumItem, "Enum." .. enumType
	end

	return rbxDomValueTypeInfo(dataType.Value, compactTypeIds)
end

rbxDomValueTypeInfo = function(valueType, compactTypeIds)
	local compactTypeKey = COMPACT_TYPE_KEY_BY_RBX_DOM_VALUE_TYPE[valueType]
	return if compactTypeKey then compactTypeIds[compactTypeKey] else nil, nil
end

local function addValueInstanceFallbacks(byClass, compactTypeIds)
	for className, valueType in pairs(VALUE_INSTANCE_VALUE_TYPES) do
		local typeId = rbxDomValueTypeInfo(valueType, compactTypeIds)
		if typeId ~= nil then
			local schema = byClass[className]
			local hasValue = false
			if type(schema) == "table" then
				for _, entry in ipairs(schema) do
					if type(entry) == "table" and tostring(entry[1]) == "Value" then
						hasValue = true
						break
					end
				end
			else
				schema = {}
				byClass[className] = schema
			end
			if not hasValue then
				table.insert(schema, { "Value", typeId, false })
				table.sort(schema, function(a, b)
					return tostring(a[1]) < tostring(b[1])
				end)
			end
		end
	end
end

local function generatedSchemaTypeInfo(typeName, compactTypeIds)
	if type(typeName) ~= "string" or typeName == "" then
		return nil, nil
	end
	local enumPrefix = "Enum."
	if string.sub(typeName, 1, #enumPrefix) == enumPrefix then
		return compactTypeIds.EnumItem, typeName
	end
	return rbxDomValueTypeInfo(typeName, compactTypeIds)
end

local function mergeGeneratedStudioApiSchema(byClass, generatedSchema, compactTypeIds)
	if type(generatedSchema) ~= "table" then
		return
	end
	for className, propertyEntries in pairs(generatedSchema) do
		if type(className) == "string" and type(propertyEntries) == "table" then
			local schema = byClass[className]
			if type(schema) ~= "table" then
				schema = {}
				byClass[className] = schema
			end
			local seen = {}
			for _, entry in ipairs(schema) do
				if type(entry) == "table" then
					seen[string.lower(tostring(entry[1] or ""))] = true
				end
			end
			for _, entry in ipairs(propertyEntries) do
				if type(entry) == "table" then
					local propertyName = tostring(entry[1] or "")
					local propertyKey = string.lower(propertyName)
					if propertyName ~= "" and not seen[propertyKey] then
						local typeId, enumType = generatedSchemaTypeInfo(entry[2], compactTypeIds)
						if typeId ~= nil then
							seen[propertyKey] = true
							schema[#schema + 1] = { propertyName, typeId, enumType or false }
						end
					end
				end
			end
			table.sort(schema, function(a, b)
				return tostring(a[1]) < tostring(b[1])
			end)
		end
	end
end

schemaPropertyNameForClass = function(className, propertyName)
	if
		(className == "Model" or className == "WorldModel")
		and propertyName == "WorldPivotData"
	then
		return "WorldPivot"
	end
	return propertyName
end

function BridgePropertySchema.buildSchemasFromRbxDom(database, compactTypeIds, generatedStudioApiSchema)
	local classes = type(database) == "table" and database.Classes or nil
	if type(classes) ~= "table" then
		error("Renium's bundled rbx-dom database is missing or invalid")
	end
	local byClass = {}

	local memo = {}
	local visiting = {}

	local function collectNamesForClass(className)
		local cached = memo[className]
		if cached then
			return cached
		end
		if visiting[className] then
			return {}
		end
		visiting[className] = true

		local names = {}
		local seen = {}
		local classData = classes[className]
		if type(classData) == "table" then
			local superclass = classData.Superclass
			if type(superclass) == "string" and superclass ~= "" then
				local inherited = collectNamesForClass(superclass)
				for _, inheritedEntry in ipairs(inherited) do
					local inheritedName = schemaPropertyNameForClass(className, tostring(inheritedEntry[1] or ""))
					local inheritedKey = string.lower(inheritedName)
					if not seen[inheritedKey] then
						seen[inheritedKey] = true
						names[#names + 1] = { inheritedName, inheritedEntry[2], inheritedEntry[3] }
					end
				end
			end

			local properties = classData.Properties
			if type(properties) == "table" then
				for propertyName, propertyData in pairs(properties) do
					if type(propertyName) == "string" then
						local lowered = string.lower(propertyName)
						if
							lowered ~= "source"
							and lowered ~= "robloxlocked"
							and not isEngineManagedStudioProperty(classes, className, propertyData)
							and isStudioEditableProperty(propertyData)
							and isRbxDomPropertyTypeSupported(propertyData)
							and not seen[lowered]
						then
							local typeId, enumType = rbxDomPropertyTypeInfo(propertyData, compactTypeIds)
							if typeId ~= nil then
								local schemaName = schemaPropertyNameForClass(className, propertyName)
								local schemaKey = string.lower(schemaName)
								if not seen[schemaKey] then
									seen[schemaKey] = true
									names[#names + 1] = { schemaName, typeId, enumType or false }
								end
							end
						end
					end
				end
			end

			local valueType = VALUE_INSTANCE_VALUE_TYPES[className]
			if type(valueType) == "string" and not seen.value then
				local typeId = rbxDomValueTypeInfo(valueType, compactTypeIds)
				if typeId ~= nil then
					seen.value = true
					names[#names + 1] = { "Value", typeId, false }
				end
			end

			addSupplementalReadableTransportProperties(classes, className, names, seen, compactTypeIds)

		end

		table.sort(names, function(a, b)
			return tostring(a[1]) < tostring(b[1])
		end)
		visiting[className] = nil
		memo[className] = names
		return names
	end

	for className in pairs(classes) do
		if type(className) == "string" then
			local names = collectNamesForClass(className)
			if #names > 0 then
				byClass[className] = names
			end
		end
	end
	mergeGeneratedStudioApiSchema(byClass, generatedStudioApiSchema, compactTypeIds)
	addValueInstanceFallbacks(byClass, compactTypeIds)
	return byClass
end

function BridgePropertySchema.buildCandidatesFromSchemas(byClass)
	local out = {}
	for className, schemaEntries in pairs(byClass) do
		local names = table.create(#schemaEntries)
		for i, schemaEntry in ipairs(schemaEntries) do
			names[i] = tostring(schemaEntry[1] or "")
		end
		out[className] = names
	end
	return out
end

function BridgePropertySchema.mergeSchemas(baseSchemas, overrideSchemas)
	local out = {}
	local indicesByClass = {}

	local function mergeSource(source, replaceExisting)
		for className, schemaEntries in pairs(source) do
			local merged = out[className]
			if not merged then
				merged = {}
				out[className] = merged
				indicesByClass[className] = {}
			end

			local indexByName = indicesByClass[className]
			for _, entry in ipairs(schemaEntries) do
				local copied = { entry[1], entry[2], entry[3] }
				local key = string.lower(copied[1])
				local existingIndex = indexByName[key]
				if existingIndex then
					if replaceExisting then
						merged[existingIndex] = copied
					end
				else
					merged[#merged + 1] = copied
					indexByName[key] = #merged
				end
			end
		end
	end

	mergeSource(baseSchemas, false)
	mergeSource(overrideSchemas, true)
	for _, merged in pairs(out) do
		table.sort(merged, function(a, b)
			return a[1] < b[1]
		end)
	end
	return out
end

function BridgePropertySchema.countCandidates(byClass)
	local classCount = 0
	local propertyCount = 0
	for _, names in pairs(byClass) do
		classCount += 1
		propertyCount += #names
	end
	return classCount, propertyCount
end

return BridgePropertySchema
