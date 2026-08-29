local BridgeInstanceSwap = {}

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

	local okCreate, replacement = pcall(createInstance or Instance.new, className)
	if not okCreate or replacement == nil then
		error(`Cannot create replacement {className} for {instance:GetFullName()}: {tostring(replacement)}`)
	end
	replacement.Name = instance.Name
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
		setParent(replacement, parent)
	end)
	if not okSwap then
		local rollbackErrors = {}
		local function rollback(operation, ...)
			local ok, result = pcall(operation, ...)
			if not ok then
				rollbackErrors[#rollbackErrors + 1] = tostring(result)
			end
		end
		rollback(setParent, replacement, nil)
		for index = #movedChildren, 1, -1 do
			rollback(setParent, movedChildren[index], instance)
		end
		rollback(setParent, instance, parent)
		rollback(replacement.Destroy, replacement)
		if #rollbackErrors > 0 then
			error(
				`Cannot replace {instance:GetFullName()} with {className}: {tostring(swapErr)}; rollback also failed: {table.concat(rollbackErrors, "; ")}`,
				0
			)
		end
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
