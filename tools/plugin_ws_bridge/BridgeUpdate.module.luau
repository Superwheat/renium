local BridgeUpdate = {}

local function parseVersion(value: any): { number }?
	if type(value) ~= "string" then
		return nil
	end
	local major, minor, patch = string.match(value, "^v?(%d+)%.(%d+)%.(%d+)$")
	if not major then
		return nil
	end
	return { tonumber(major), tonumber(minor), tonumber(patch) }
end

function BridgeUpdate.isNewer(candidate: any, current: any): boolean
	local nextVersion = parseVersion(candidate)
	local currentVersion = parseVersion(current)
	if not nextVersion or not currentVersion then
		return false
	end
	for index = 1, 3 do
		if nextVersion[index] ~= currentVersion[index] then
			return nextVersion[index] > currentVersion[index]
		end
	end
	return false
end

return BridgeUpdate
