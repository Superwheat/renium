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

function BridgeInstanceSwap.replaceChildren(
	groups: { any },
	incomingByGroup: { { Instance } },
	assignParent: (Instance, Instance?) -> ()
): { any }
	local outgoingByGroup = {}
	for groupIndex, group in ipairs(groups) do
		local outgoing = {}
		for _, child in ipairs(group.target:GetChildren()) do
			if not group.preserved[child] then
				outgoing[#outgoing + 1] = child
			end
		end
		outgoingByGroup[groupIndex] = outgoing
	end

	local removed = {}
	local parented = {}
	local ok, swapError = pcall(function()
		for groupIndex, group in ipairs(groups) do
			for _, instance in ipairs(incomingByGroup[groupIndex]) do
				assignParent(instance, group.target)
				parented[#parented + 1] = instance
			end
		end
		for groupIndex, group in ipairs(groups) do
			for _, child in ipairs(outgoingByGroup[groupIndex]) do
				assignParent(child, nil)
				removed[#removed + 1] = { instance = child, parent = group.target }
			end
		end
	end)
	if ok then
		return removed
	end

	local rollbackError
	for index = #removed, 1, -1 do
		local restored, result = pcall(assignParent, removed[index].instance, removed[index].parent)
		if not restored and rollbackError == nil then
			rollbackError = result
		end
	end
	for index = #parented, 1, -1 do
		local restored, result = pcall(assignParent, parented[index], nil)
		if not restored and rollbackError == nil then
			rollbackError = result
		end
	end
	if rollbackError ~= nil then
		error(`{swapError}; rollback also failed: {rollbackError}`, 0)
	end
	error(swapError, 0)
end

return BridgeInstanceSwap
