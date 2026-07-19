local BridgeReferenceRetarget = {}

function BridgeReferenceRetarget.apply(
	roots: { Instance },
	replacements: { [Instance]: Instance },
	getPropertyNames: (string) -> { string },
	readProperty: (Instance, string) -> (boolean, any),
	writeProperty: (Instance, string, any) -> (boolean, any)
): (number, number)
	if next(replacements) == nil then
		return 0, 0
	end
	local updated = 0
	local failed = 0
	for _, root in ipairs(roots) do
		local instances = { root }
		for _, descendant in ipairs(root:GetDescendants()) do
			instances[#instances + 1] = descendant
		end
		for _, instance in ipairs(instances) do
			for _, propertyName in ipairs(getPropertyNames(instance.ClassName)) do
				local okRead, current = readProperty(instance, propertyName)
				local replacement = if okRead then replacements[current] else nil
				if replacement ~= nil then
					local okWrite = writeProperty(instance, propertyName, replacement)
					if okWrite then
						updated += 1
					else
						failed += 1
					end
				end
			end
		end
	end
	return updated, failed
end

return BridgeReferenceRetarget
