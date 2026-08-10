local BridgeInstanceSwap = {}

local function canReparent(instance: Instance): boolean
	local okRead, locked = pcall(function()
		return (instance :: any).RobloxLocked
	end)
	return okRead and not locked
end

local function restoreOrder(
	parent: Instance,
	desired: { Instance },
	assignParent: ((Instance, Instance?) -> ())?
): boolean
	local desiredSet = {}
	local order = table.clone(desired)
	local current = parent:GetChildren()
	for _, child in ipairs(desired) do
		desiredSet[child] = true
	end
	for _, child in ipairs(current) do
		if not desiredSet[child] then
			order[#order + 1] = child
		end
	end

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
		if assignParent ~= nil then
			assignParent(current[index], nil)
		else
			current[index].Parent = nil
		end
	end
	for index = prefix + 1, #order do
		if assignParent ~= nil then
			assignParent(order[index], parent)
		else
			order[index].Parent = parent
		end
	end
	return true
end

function BridgeInstanceSwap.replace(
	instance: Instance,
	className: string,
	collectionService: any,
	removeInstance: (Instance) -> (),
	createInstance: ((string) -> Instance)?,
	assignParent: ((Instance, Instance?) -> ())?
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
		error(`Cannot create replacement {className} for {instance:GetFullName()}: {tostring(replacement)}`)
	end
	replacement.Name = instance.Name
	for index = siblingIndex + 1, #originalSiblings do
		if not canReparent(originalSiblings[index]) then
			replacement:Destroy()
			error(`Cannot preserve sibling order while replacing {instance:GetFullName()} with {className}`)
		end
	end

	local desiredSiblings = table.clone(originalSiblings)
	desiredSiblings[siblingIndex] = replacement
	local movedChildren = {}
	local function setParent(target: Instance, nextParent: Instance?)
		if assignParent ~= nil then
			assignParent(target, nextParent)
		else
			target.Parent = nextParent
		end
	end
	local okSwap, swapErr = pcall(function()
		for attributeName, attributeValue in pairs(instance:GetAttributes()) do
			replacement:SetAttribute(attributeName, attributeValue)
		end
		for _, tag in ipairs(collectionService:GetTags(instance)) do
			collectionService:AddTag(replacement, tag)
		end
		for _, child in ipairs(instance:GetChildren()) do
			setParent(child, replacement)
			movedChildren[#movedChildren + 1] = child
		end
		removeInstance(instance)
		if not restoreOrder(parent, desiredSiblings, assignParent) then
			error("Cannot restore sibling order after class replacement")
		end
	end)
	if not okSwap then
		for index = #movedChildren, 1, -1 do
			pcall(setParent, movedChildren[index], instance)
		end
		pcall(function()
			setParent(replacement, nil)
			setParent(instance, parent)
			restoreOrder(parent, originalSiblings, assignParent)
		end)
		replacement:Destroy()
		error(`Cannot replace {instance:GetFullName()} with {className}: {tostring(swapErr)}`, 0)
	end

	return replacement
end

return BridgeInstanceSwap
