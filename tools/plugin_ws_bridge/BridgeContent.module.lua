local BridgeContent = {}

function BridgeContent.serializeSource(sourceType, uri)
	local name
	if type(sourceType) == "string" then
		name = sourceType
	else
		name = sourceType.Name
	end
	if name == "Uri" then
		return uri or ""
	end
	if name == "None" then
		return ""
	end
	error("Renium cannot serialize Content." .. tostring(name) .. " values without losing data")
end

function BridgeContent.serialize(value)
	return BridgeContent.serializeSource(value.SourceType, value.Uri)
end

return BridgeContent
