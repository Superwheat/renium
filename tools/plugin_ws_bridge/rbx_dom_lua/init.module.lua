local database = require(script.database)
local Error = require(script.Error)
local PropertyDescriptor = require(script.PropertyDescriptor)

local referencePropertiesByClass = {}
local objectContentPropertiesByClass = {}

local function getPropertyNames(className, cache, dataType, skipParent)
	local cached = cache[className]
	if cached ~= nil then
		return cached
	end
	local names = {}
	local seen = {}
	local currentClassName = className
	repeat
		local currentClass = database.Classes[currentClassName]
		if currentClass == nil then
			break
		end
		for propertyName, propertyData in pairs(currentClass.Properties) do
			if seen[propertyName] == nil then
				seen[propertyName] = true
				local scriptability = propertyData.Scriptability
				if
					propertyData.Kind.Canonical ~= nil
					and propertyData.DataType.Value == dataType
					and (scriptability == "ReadWrite" or scriptability == "Custom")
					and (not skipParent or propertyName ~= "Parent")
				then
					names[#names + 1] = propertyName
				end
			end
		end
		currentClassName = currentClass.Superclass
	until currentClassName == nil
	table.sort(names)
	cache[className] = names
	return names
end

local function getReferencePropertyNames(className)
	return getPropertyNames(className, referencePropertiesByClass, "Ref", true)
end

local function getObjectContentPropertyNames(className)
	return getPropertyNames(className, objectContentPropertiesByClass, "Content", false)
end

local function findCanonicalPropertyDescriptor(className, propertyName)
	local currentClassName = className

	repeat
		local currentClass = database.Classes[currentClassName]

		if currentClass == nil then
			return currentClass
		end

		local propertyData = currentClass.Properties[propertyName]
		if propertyData ~= nil then
			local canonicalData = propertyData.Kind.Canonical
			if canonicalData ~= nil then
				return PropertyDescriptor.fromRaw(propertyData, currentClassName, propertyName)
			end

			local aliasData = propertyData.Kind.Alias
			if aliasData ~= nil then
				return PropertyDescriptor.fromRaw(
					currentClass.Properties[aliasData.AliasFor],
					currentClassName,
					aliasData.AliasFor
				)
			end

			return nil
		end

		currentClassName = currentClass.Superclass
	until currentClassName == nil

	return nil
end

local function readProperty(instance, propertyName)
	local descriptor = findCanonicalPropertyDescriptor(instance.ClassName, propertyName)

	if descriptor == nil then
		local fullName = ("%s.%s"):format(instance.ClassName, propertyName)

		return false, Error.new(Error.Kind.UnknownProperty, fullName)
	end

	return descriptor:read(instance)
end

local function writeProperty(instance, propertyName, value)
	local descriptor = findCanonicalPropertyDescriptor(instance.ClassName, propertyName)

	if descriptor == nil then
		local fullName = ("%s.%s"):format(instance.ClassName, propertyName)

		return false, Error.new(Error.Kind.UnknownProperty, fullName)
	end

	return descriptor:write(instance, value)
end

return {
	readProperty = readProperty,
	writeProperty = writeProperty,
	findCanonicalPropertyDescriptor = findCanonicalPropertyDescriptor,
	getReferencePropertyNames = getReferencePropertyNames,
	getObjectContentPropertyNames = getObjectContentPropertyNames,
	Error = Error,
	EncodedValue = require(script.EncodedValue),
}
