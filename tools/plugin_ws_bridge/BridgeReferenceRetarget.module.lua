local BridgeReferenceRetarget = {}

function BridgeReferenceRetarget.apply(
	roots: { Instance },
	replacements: { [Instance]: Instance },
	getPropertyNames: (string) -> { string },
	readProperty: (Instance, string) -> (boolean, any),
	writeProperty: (Instance, string, any) -> (boolean, any),
	excludedRoots: { [Instance]: boolean }?
): (number, number, { { instance: Instance, propertyName: string, error: any } })
	if next(replacements) == nil and getmetatable(replacements) == nil then
		return 0, 0, {}
	end
	local updated = 0
	local failed = 0
	local failures = {}
	for _, root in ipairs(roots) do
		local pending = { root }
		while #pending > 0 do
			local instance = table.remove(pending)
			if excludedRoots ~= nil and excludedRoots[instance] then
				continue
			end
			for _, child in ipairs(instance:GetChildren()) do
				pending[#pending + 1] = child
			end
			for _, propertyName in ipairs(getPropertyNames(instance.ClassName)) do
				local okRead, current = readProperty(instance, propertyName)
				local replacement = if okRead then replacements[current] else nil
				if replacement ~= nil then
					local okWrite, writeError = writeProperty(instance, propertyName, replacement)
					if okWrite then
						updated += 1
					else
						failed += 1
						failures[#failures + 1] = {
							instance = instance,
							propertyName = propertyName,
							error = writeError,
						}
					end
				end
			end
		end
	end
	return updated, failed, failures
end

return BridgeReferenceRetarget
