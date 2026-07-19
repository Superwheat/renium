local BridgeInstanceSwap = {}

local function canReparent(instance: Instance): boolean
	local okRead, locked = pcall(function()
		return (instance :: any).RobloxLocked
	end)
	return okRead and locked ~= true
end

local function restoreOrder(parent: Instance, desired: { Instance }): boolean
	local desiredSet = {}
	local order = table.clone(desired)
	for _, child in ipairs(desired) do
		desiredSet[child] = true
	end
	for _, child in ipairs(parent:GetChildren()) do
		if desiredSet[child] ~= true then
			order[#order + 1] = child
		end
	end

	local current = parent:GetChildren()
	local prefix = 0
	while prefix < #current and prefix < #order and current[prefix + 1] == order[prefix + 1] do
		prefix += 1
	end
	if prefix == #current and prefix == #order then
		return true
	end
	for index = prefix + 1, #current do
		if not canReparent(current[index]) then
			return false
		end
	end

	for index = prefix + 1, #current do
		current[index].Parent = nil
	end
	for index = prefix + 1, #order do
		order[index].Parent = parent
	end
	return true
end

function BridgeInstanceSwap.replace(
	instance: Instance,
	className: string,
	collectionService: any,
	removeInstance: (Instance) -> (),
	createInstance: ((string) -> Instance)?
): Instance
	local parent = instance.Parent
	if parent == nil then
		error("Cannot replace service root class for " .. instance:GetFullName())
	end
	local originalSiblings = parent:GetChildren()
	local siblingIndex = table.find(originalSiblings, instance)
	if siblingIndex == nil then
		error("Cannot locate replacement target under " .. parent:GetFullName())
	end

	local okCreate, replacement = pcall(createInstance or Instance.new, className)
	if not okCreate or replacement == nil then
		error("Cannot create replacement " .. className .. " for " .. instance:GetFullName() .. ": " .. tostring(replacement))
	end
	replacement.Name = instance.Name

	local okAttributes, attributes = pcall(instance.GetAttributes, instance)
	if okAttributes and type(attributes) == "table" then
		for attributeName, attributeValue in pairs(attributes) do
			pcall(replacement.SetAttribute, replacement, attributeName, attributeValue)
		end
	end
	for _, tag in ipairs(collectionService:GetTags(instance)) do
		pcall(collectionService.AddTag, collectionService, replacement, tag)
	end

	local desiredSiblings = table.clone(originalSiblings)
	desiredSiblings[siblingIndex] = replacement
	local movedChildren = {}
	local okSwap, swapErr = pcall(function()
		for _, child in ipairs(instance:GetChildren()) do
			child.Parent = replacement
			movedChildren[#movedChildren + 1] = child
		end
		removeInstance(instance)
		if not restoreOrder(parent, desiredSiblings) then
			replacement.Parent = parent
		end
	end)
	if not okSwap then
		for index = #movedChildren, 1, -1 do
			pcall(function()
				movedChildren[index].Parent = instance
			end)
		end
		pcall(function()
			replacement.Parent = nil
			instance.Parent = parent
			restoreOrder(parent, originalSiblings)
		end)
		pcall(replacement.Destroy, replacement)
		error("Cannot replace " .. instance:GetFullName() .. " with " .. className .. ": " .. tostring(swapErr), 0)
	end

	return replacement
end

return BridgeInstanceSwap
