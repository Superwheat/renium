local StudioService = game:GetService("StudioService")

local AXIS_NAMES = { "X", "Y", "Z" }
local FACE_NAMES = { "Right", "Top", "Back", "Left", "Bottom", "Front" }
local CFRAME_COMPONENT_NAMES = { "X", "Y", "Z", "R00", "R01", "R02", "R10", "R11", "R12", "R20", "R21", "R22" }

local function escapeRich(text)
	text = tostring(text)
	text = string.gsub(text, "&", "&amp;")
	text = string.gsub(text, "<", "&lt;")
	text = string.gsub(text, ">", "&gt;")
	return text
end

local function colorHex(color)
	return string.format(
		"#%02X%02X%02X",
		math.floor(color.R * 255 + 0.5),
		math.floor(color.G * 255 + 0.5),
		math.floor(color.B * 255 + 0.5)
	)
end

local function truncateValueText(text)
	local okLength, length = pcall(utf8.len, text)
	if okLength and length and length > 48 then
		local okOffset, offset = pcall(utf8.offset, text, 46)
		if okOffset and offset then
			return string.sub(text, 1, offset - 1) .. "\226\128\166"
		end
	end
	if #text > 48 then
		return string.sub(text, 1, 45) .. "..."
	end
	return text
end

local function formatStringValue(value)
	local okUtf8, length = pcall(utf8.len, value)
	if not okUtf8 or not length then
		return string.format("Binary data (%d bytes)", #value)
	end
	for index = 1, #value do
		local byte = string.byte(value, index)
		if byte < 32 and byte ~= 9 and byte ~= 10 and byte ~= 13 then
			return string.format("Binary data (%d bytes)", #value)
		end
	end
	return truncateValueText(value)
end

local function shortFontFamily(value)
	local family = tostring(value or "")
	local name = string.match(family, "([^/]+)%.json$") or string.match(family, "([^/]+)$") or family
	return name ~= "" and name or "Default"
end

local function formatFontValue(value)
	if typeof(value) == "Font" then
		return string.format("%s, %s, %s", shortFontFamily(value.Family), value.Weight.Name, value.Style.Name)
	end
	return string.format(
		"%s, %s, %s",
		shortFontFamily(value.family or value.Family),
		(tostring(value.weight or value.Weight or "Regular"):gsub("^Enum%.FontWeight%.", "")),
		(tostring(value.style or value.Style or "Normal"):gsub("^Enum%.FontStyle%.", ""))
	)
end

local function formatPushValue(raw)
	local kind = typeof(raw)
	if kind == "boolean" or kind == "number" then
		return tostring(raw)
	elseif kind == "string" then
		return formatStringValue(raw)
	elseif kind == "table" then
		if raw.family ~= nil or raw.Family ~= nil then
			return formatFontValue(raw)
		end
		local tag = tostring(raw._type or "")
		if tag == "Color3" then
			return string.format(
				"%d, %d, %d",
				math.floor((tonumber(raw.r) or 0) * 255 + 0.5),
				math.floor((tonumber(raw.g) or 0) * 255 + 0.5),
				math.floor((tonumber(raw.b) or 0) * 255 + 0.5)
			)
		elseif tag == "Vector3" then
			return string.format(
				"%s, %s, %s",
				tostring(tonumber(raw.x) or 0),
				tostring(tonumber(raw.y) or 0),
				tostring(tonumber(raw.z) or 0)
			)
		elseif tag == "Vector2" then
			return string.format("%s, %s", tostring(tonumber(raw.x) or 0), tostring(tonumber(raw.y) or 0))
		elseif tag == "EnumItem" then
			local value = tostring(raw.name or raw.value or "")
			return string.match(value, "[^%.]+$") or value
		elseif tag == "Float" then
			return tostring(raw.value)
		elseif tag ~= "" then
			return tag
		end
		return "Structured value"
	end
	return tostring(raw)
end

local function formatInstancePath(instance)
	local segments = {}
	local current = instance
	while current and current ~= game do
		local name = current.Name
		local parent = current.Parent
		if parent then
			local matching = 0
			local ordinal = 0
			for _, sibling in ipairs(parent:GetChildren()) do
				if sibling.Name == name then
					matching += 1
					if sibling == current then
						ordinal = matching
					end
				end
			end
			if matching > 1 then
				name = string.format("%s[%d]", name, ordinal)
			end
		end
		segments[#segments + 1] = name
		current = parent
	end
	for left = 1, math.floor(#segments / 2) do
		local right = #segments - left + 1
		segments[left], segments[right] = segments[right], segments[left]
	end
	return table.concat(segments, ".")
end

local function formatStructuredPath(segments, ordinals, lastIndex)
	local parts = {}
	for index = 1, math.min(lastIndex or #segments, #segments) do
		local name = tostring(segments[index])
		local ordinal = type(ordinals) == "table" and tonumber(ordinals[index]) or 1
		if ordinal ~= nil and ordinal > 1 then
			name = string.format("%s[%d]", name, ordinal)
		end
		table.insert(parts, name)
	end
	return table.concat(parts, ".")
end

local function formatLiveValue(value)
	local kind = typeof(value)
	if kind == "nil" then
		return nil
	end
	if kind == "boolean" then
		return tostring(value)
	end
	if kind == "number" then
		return string.format("%.6g", value)
	end
	if kind == "string" then
		return formatStringValue(value)
	end
	if kind == "Color3" then
		return string.format(
			"%d, %d, %d",
			math.floor(value.R * 255 + 0.5),
			math.floor(value.G * 255 + 0.5),
			math.floor(value.B * 255 + 0.5)
		)
	end
	if kind == "Vector3" then
		return string.format("%.6g, %.6g, %.6g", value.X, value.Y, value.Z)
	end
	if kind == "Vector2" then
		return string.format("%.6g, %.6g", value.X, value.Y)
	end
	if kind == "EnumItem" then
		return value.Name
	end
	if kind == "Instance" then
		return truncateValueText(formatInstancePath(value))
	end
	if kind == "BrickColor" then
		return value.Name
	end
	if kind == "UDim2" then
		return tostring(value)
	end
	if kind == "PhysicalProperties" then
		return string.format(
			"%.6g, %.6g, %.6g, %.6g, %.6g, %.6g",
			value.Density,
			value.Friction,
			value.Elasticity,
			value.FrictionWeight,
			value.ElasticityWeight,
			value.AcousticAbsorption
		)
	end
	if kind == "Font" then
		return formatFontValue(value)
	end
	if kind == "CFrame" then
		return tostring(value)
	end
	if kind == "ColorSequence" then
		local parts = table.create(#value.Keypoints)
		for index, keypoint in ipairs(value.Keypoints) do
			parts[index] = string.format(
				"%.6g:(%.6g, %.6g, %.6g)",
				keypoint.Time,
				keypoint.Value.R,
				keypoint.Value.G,
				keypoint.Value.B
			)
		end
		return table.concat(parts, "; ")
	end
	if kind == "NumberSequence" then
		local parts = table.create(#value.Keypoints)
		for index, keypoint in ipairs(value.Keypoints) do
			parts[index] = string.format("%.6g:(%.6g, %.6g)", keypoint.Time, keypoint.Value, keypoint.Envelope)
		end
		return table.concat(parts, "; ")
	end
	if kind == "Axes" then
		local parts = {}
		for _, name in ipairs(AXIS_NAMES) do
			if value[name] then
				table.insert(parts, name)
			end
		end
		return #parts > 0 and table.concat(parts, ", ") or "None"
	end
	if kind == "Faces" then
		local parts = {}
		for _, name in ipairs(FACE_NAMES) do
			if value[name] then
				table.insert(parts, name)
			end
		end
		return #parts > 0 and table.concat(parts, ", ") or "None"
	end
	if kind == "Ray" then
		return string.format(
			"Origin %.6g, %.6g, %.6g; Direction %.6g, %.6g, %.6g",
			value.Origin.X,
			value.Origin.Y,
			value.Origin.Z,
			value.Direction.X,
			value.Direction.Y,
			value.Direction.Z
		)
	end
	if kind == "UDim" or kind == "NumberRange" or kind == "Rect" then
		return tostring(value)
	end
	if kind == "table" then
		if value.family ~= nil or value.Family ~= nil then
			return formatFontValue(value)
		end
		if value.density then
			return string.format(
				"%.6g, %.6g, %.6g",
				tonumber(value.density) or 0,
				tonumber(value.friction) or 0,
				tonumber(value.elasticity) or 0
			)
		end
		if value.customPhysics == false then
			return "Default"
		end
		local tag = tostring(value._type or "")
		if tag ~= "" then
			return tag
		end
		return "Structured value"
	end
	return kind
end

local function reviewValueParts(value)
	local kind = typeof(value)
	if kind == "Vector3" then
		return { { "X", value.X }, { "Y", value.Y }, { "Z", value.Z } }
	end
	if kind == "Vector2" then
		return { { "X", value.X }, { "Y", value.Y } }
	end
	if kind == "Color3" then
		return {
			{ "R", math.floor(value.R * 255 + 0.5) },
			{ "G", math.floor(value.G * 255 + 0.5) },
			{ "B", math.floor(value.B * 255 + 0.5) },
		}
	end
	if kind == "UDim" then
		return { { "Scale", value.Scale }, { "Offset", value.Offset } }
	end
	if kind == "UDim2" then
		return {
			{ "X Scale", value.X.Scale },
			{ "X Offset", value.X.Offset },
			{ "Y Scale", value.Y.Scale },
			{ "Y Offset", value.Y.Offset },
		}
	end
	if kind == "NumberRange" then
		return { { "Min", value.Min }, { "Max", value.Max } }
	end
	if kind == "Rect" then
		return {
			{ "Min X", value.Min.X },
			{ "Min Y", value.Min.Y },
			{ "Max X", value.Max.X },
			{ "Max Y", value.Max.Y },
		}
	end
	if kind == "PhysicalProperties" then
		return {
			{ "Density", value.Density },
			{ "Friction", value.Friction },
			{ "Elasticity", value.Elasticity },
			{ "Friction weight", value.FrictionWeight },
			{ "Elasticity weight", value.ElasticityWeight },
			{ "Acoustic absorption", value.AcousticAbsorption },
		}
	end
	if kind == "CFrame" then
		local components = { value:GetComponents() }
		local parts = table.create(12)
		for index, name in ipairs(CFRAME_COMPONENT_NAMES) do
			parts[index] = { name, components[index] }
		end
		return parts
	end
	if kind == "ColorSequence" then
		local parts = {}
		for index, keypoint in ipairs(value.Keypoints) do
			local prefix = string.format("Keypoint %d", index)
			table.insert(parts, { prefix .. " time", keypoint.Time })
			table.insert(parts, { prefix .. " red", keypoint.Value.R })
			table.insert(parts, { prefix .. " green", keypoint.Value.G })
			table.insert(parts, { prefix .. " blue", keypoint.Value.B })
		end
		return parts
	end
	if kind == "NumberSequence" then
		local parts = {}
		for index, keypoint in ipairs(value.Keypoints) do
			local prefix = string.format("Keypoint %d", index)
			table.insert(parts, { prefix .. " time", keypoint.Time })
			table.insert(parts, { prefix .. " value", keypoint.Value })
			table.insert(parts, { prefix .. " envelope", keypoint.Envelope })
		end
		return parts
	end
	if kind == "Axes" then
		return { { "X", value.X }, { "Y", value.Y }, { "Z", value.Z } }
	end
	if kind == "Faces" then
		return {
			{ "Right", value.Right },
			{ "Top", value.Top },
			{ "Back", value.Back },
			{ "Left", value.Left },
			{ "Bottom", value.Bottom },
			{ "Front", value.Front },
		}
	end
	if kind == "Ray" then
		return {
			{ "Origin X", value.Origin.X },
			{ "Origin Y", value.Origin.Y },
			{ "Origin Z", value.Origin.Z },
			{ "Direction X", value.Direction.X },
			{ "Direction Y", value.Direction.Y },
			{ "Direction Z", value.Direction.Z },
		}
	end
	if kind == "Font" then
		return {
			{ "Family", shortFontFamily(value.Family) },
			{ "Weight", value.Weight.Name },
			{ "Style", value.Style.Name },
		}
	end
	if kind ~= "table" then
		return nil
	end
	if value.family ~= nil or value.Family ~= nil then
		return {
			{ "Family", shortFontFamily(value.family or value.Family) },
			{ "Weight", (tostring(value.weight or value.Weight or "Regular"):gsub("^Enum%.FontWeight%.", "")) },
			{ "Style", (tostring(value.style or value.Style or "Normal"):gsub("^Enum%.FontStyle%.", "")) },
		}
	end
	local tag = tostring(value._type or "")
	if tag == "Vector3" or tag == "Vector2" then
		local parts = { { "X", value.x }, { "Y", value.y } }
		if tag == "Vector3" then
			table.insert(parts, { "Z", value.z })
		end
		return parts
	end
	if tag == "Color3" then
		return {
			{ "R", math.floor((tonumber(value.r) or 0) * 255 + 0.5) },
			{ "G", math.floor((tonumber(value.g) or 0) * 255 + 0.5) },
			{ "B", math.floor((tonumber(value.b) or 0) * 255 + 0.5) },
		}
	end
	if tag == "UDim" then
		return { { "Scale", value.scale }, { "Offset", value.offset } }
	end
	if tag == "UDim2" then
		return {
			{ "X Scale", value.xScale },
			{ "X Offset", value.xOffset },
			{ "Y Scale", value.yScale },
			{ "Y Offset", value.yOffset },
		}
	end
	if tag == "NumberRange" or value.min ~= nil and value.max ~= nil then
		return { { "Min", value.min }, { "Max", value.max } }
	end
	if value.density then
		return {
			{ "Density", value.density },
			{ "Friction", value.friction },
			{ "Elasticity", value.elasticity },
			{ "Friction weight", value.frictionWeight },
			{ "Elasticity weight", value.elasticityWeight },
			{ "Acoustic absorption", value.acousticAbsorption },
		}
	end
	return nil
end

local function reviewValueComponents(oldValue, newValue)
	local oldParts = reviewValueParts(oldValue)
	local newParts = reviewValueParts(newValue)
	if not oldParts and not newParts then
		return nil
	end
	local oldByName = {}
	local newByName = {}
	local order = {}
	local seen = {}
	for _, part in ipairs(oldParts or {}) do
		oldByName[part[1]] = part[2]
		if not seen[part[1]] then
			seen[part[1]] = true
			table.insert(order, part[1])
		end
	end
	for _, part in ipairs(newParts or {}) do
		newByName[part[1]] = part[2]
		if not seen[part[1]] then
			seen[part[1]] = true
			table.insert(order, part[1])
		end
	end
	local components = {}
	for _, name in ipairs(order) do
		local oldPart = oldByName[name]
		local newPart = newByName[name]
		if oldPart ~= newPart then
			table.insert(components, {
				name = name,
				oldText = oldPart ~= nil and formatLiveValue(oldPart) or nil,
				newText = newPart ~= nil and formatLiveValue(newPart) or "Not set",
			})
		end
	end
	return #components > 0 and components or nil
end

local reviewIconCache = {}

local function reviewClassIconData(className)
	local cached = reviewIconCache[className]
	if cached then
		return cached
	end
	local ok, rawData = pcall(StudioService.GetClassIcon, StudioService, className)
	local data = if ok and type(rawData) == "table" then rawData else {}
	reviewIconCache[className] = data
	return data
end

local function compareReviewNodes(a, b)
	local an = string.lower(a.name)
	local bn = string.lower(b.name)
	local ap, ad = string.match(an, "^(.-)(%d+)$")
	local bp, bd = string.match(bn, "^(.-)(%d+)$")
	if ap and bp and ap == bp then
		return tonumber(ad) < tonumber(bd)
	end
	return an < bn
end

local function findOrdinalChild(parent, name, ordinal)
	if not parent then
		return nil
	end
	if ordinal <= 1 then
		return parent:FindFirstChild(name)
	end
	local seen = 0
	for _, child in ipairs(parent:GetChildren()) do
		if child.Name == name then
			seen = seen + 1
			if seen == ordinal then
				return child
			end
		end
	end
	return nil
end

local function newReviewNode(name, className, instance, ordinal)
	return {
		name = name,
		displayName = name,
		ordinal = ordinal or 1,
		className = className,
		instance = instance,
		children = {},
		childByKey = {},
		props = {},
		expanded = false,
		changeTotal = 0,
	}
end

local function buildReviewTree(summaryRows, groups, helpers)
	local roots = {}
	local rootByName = {}

	for _, row in ipairs(summaryRows) do
		local serviceName = tostring(row.service or "")
		local count = tonumber(row.count) or 0
		local note =
			string.format("%d instances%s", count, row.allowDeletes == true and " · may remove instances" or "")
		local node = newReviewNode("Reconcile " .. serviceName, serviceName, nil)
		node.summary = true
		node.note = note
		node.changeTotal = count
		table.insert(roots, node)
	end

	local authoritativeCounts = {}
	local authoritativeOrder = {}
	for _, group in ipairs(groups) do
		for _, entry in ipairs(group.entries) do
			if entry.kind == "instanceReconcile" and entry.allowDeletes == true then
				local serviceName = tostring(group.service or group.pathSegments[1] or "")
				if authoritativeCounts[serviceName] == nil then
					authoritativeCounts[serviceName] = 0
					table.insert(authoritativeOrder, serviceName)
				end
				authoritativeCounts[serviceName] += 1
			end
		end
	end
	for _, serviceName in ipairs(authoritativeOrder) do
		local count = authoritativeCounts[serviceName]
		local node = newReviewNode("Authoritative reconcile " .. serviceName, serviceName, nil)
		node.summary = true
		node.note = string.format("%d desired instances · may remove unmatched Studio instances", count)
		node.changeTotal = count
		table.insert(roots, node)
	end

	local function serviceNode(serviceName)
		local node = rootByName[serviceName]
		if not node then
			node = newReviewNode(serviceName, serviceName, game:GetService(serviceName))
			rootByName[serviceName] = node
			table.insert(roots, node)
		end
		return node
	end

	for _, group in ipairs(groups) do
		local segments = group.pathSegments
		local node = serviceNode(tostring(segments[1]))
		local resolvedInstance = nil
		local resolved = helpers.resolveInstance(group)
		if typeof(resolved) == "Instance" then
			resolvedInstance = resolved
		end
		local desiredParentNode = node
		for i = 2, #segments do
			local name = tostring(segments[i])
			local ordinal = if type(group.pathOrdinals) == "table" then tonumber(group.pathOrdinals[i]) or 1 else 1
			local key = name .. "\1" .. tostring(ordinal)
			local child = node.childByKey[key]
			if not child then
				local liveChild = i == #segments and resolvedInstance or findOrdinalChild(node.instance, name, ordinal)
				local className = if liveChild
					then liveChild.ClassName
					elseif i == #segments then tostring(group.className or "Folder")
					else "Folder"
				child = newReviewNode(name, className, liveChild, ordinal)
				node.childByKey[key] = child
				table.insert(node.children, child)
				local duplicateCount = 0
				for _, existing in ipairs(node.children) do
					if existing.name == name then
						duplicateCount += 1
					end
				end
				local liveDuplicateCount = 0
				if node.instance then
					for _, sibling in ipairs(node.instance:GetChildren()) do
						if sibling.Name == name then
							liveDuplicateCount += 1
						end
					end
				end
				if duplicateCount > 1 or liveDuplicateCount > 1 or ordinal > 1 then
					for _, existing in ipairs(node.children) do
						if existing.name == name then
							existing.displayName = string.format("%s[%d]", name, existing.ordinal)
						end
					end
				end
			elseif i == #segments then
				if resolvedInstance then
					child.instance = resolvedInstance
					child.className = resolvedInstance.ClassName
				elseif not child.instance then
					child.className = tostring(group.className or child.className)
				end
			end
			if i == #segments then
				desiredParentNode = node
			end
			node = child
		end
		local keepsInstance = false
		for _, entry in ipairs(group.entries) do
			if entry.kind ~= "instanceRemove" and not (entry.kind == "source" and entry.deleted == true) then
				keepsInstance = true
				break
			end
		end
		if resolvedInstance and keepsInstance then
			local desiredName = tostring(segments[#segments] or "")
			if resolvedInstance.Name ~= desiredName then
				table.insert(node.props, {
					name = "Name",
					typeName = "string",
					oldText = resolvedInstance.Name,
					newText = desiredName,
				})
			end
			local desiredParent = desiredParentNode.instance
			if desiredParent ~= nil and resolvedInstance.Parent ~= desiredParent then
				table.insert(node.props, {
					name = "Parent",
					typeName = "Instance",
					oldText = resolvedInstance.Parent ~= nil and formatInstancePath(resolvedInstance.Parent)
						or "Not set",
					newText = formatInstancePath(desiredParent),
				})
			elseif desiredParent == nil and #segments > 1 then
				local currentParentPath = resolvedInstance.Parent ~= nil and formatInstancePath(resolvedInstance.Parent)
					or "Not set"
				local desiredParentPath = formatStructuredPath(segments, group.pathOrdinals, #segments - 1)
				if currentParentPath ~= desiredParentPath then
					table.insert(node.props, {
						name = "Parent",
						typeName = "Instance",
						oldText = currentParentPath,
						newText = desiredParentPath,
					})
				end
			end
		end
		for _, entry in ipairs(group.entries) do
			if entry.kind == "instanceRemove" or (entry.kind == "source" and entry.deleted == true) then
				node.status = "removed"
			elseif entry.kind == "instanceReconcile" then
				local className = tostring(group.className or node.className)
				if not node.instance then
					node.status = "added"
				elseif node.instance.ClassName ~= className then
					table.insert(node.props, {
						name = "ClassName",
						typeName = "string",
						oldText = node.instance.ClassName,
						newText = className,
					})
				end
			elseif entry.kind == "instanceAdd" and not node.instance and not node.status then
				node.status = "added"
			elseif entry.kind == "instanceReplace" then
				local newClass = tostring(group.className or "")
				if not node.instance then
					if not node.status then
						node.status = "added"
					end
				elseif newClass == "Folder" and node.instance:IsA("LuaSourceContainer") then
					node.status = "removed"
				elseif newClass ~= "" and node.instance.ClassName ~= newClass then
					table.insert(node.props, {
						name = "ClassName",
						typeName = "string",
						oldText = node.instance.ClassName,
						newText = newClass,
					})
				end
			elseif entry.kind == "source" then
				node.sourceEdited = true
			elseif entry.kind == "property" or entry.kind == "attribute" then
				local name = tostring(entry.name or "")
				local haveOld = false
				local oldValue = nil
				local oldValueMissing = entry.oldValueMissing == true
				if entry.oldValueKnown == true then
					if oldValueMissing then
						haveOld = true
					else
						local okCall, okDecode, decoded =
							pcall(helpers.decodeValue, entry.oldValue, name, group.service)
						if okCall and okDecode then
							haveOld = true
							oldValue = decoded
						end
					end
				elseif node.instance then
					local instance = node.instance
					if entry.kind == "attribute" then
						haveOld, oldValue = pcall(instance.GetAttribute, instance, name)
					else
						local okCall, okRead, value = pcall(helpers.readProperty, instance, name)
						if okCall and okRead then
							haveOld = true
							oldValue = value
						end
					end
					if haveOld and oldValue == nil then
						oldValueMissing = true
					end
				end
				local haveNew = false
				local newValue = nil
				local deleting = entry.deleted == true
				local truncated = type(entry.value) == "table" and entry.value._reviewTruncated == true
				if deleting then
					haveNew = true
				elseif not truncated then
					local okCall, okDecode, decoded = pcall(helpers.decodeValue, entry.value, name, group.service)
					if okCall and okDecode then
						haveNew = true
						newValue = decoded
					end
				end
				local isNoop = haveOld and haveNew and helpers.valuesEqual(oldValue, newValue)
					or oldValue == nil and type(newValue) == "table" and newValue.customPhysics == false
				if not isNoop then
					local oldText = haveOld and (oldValueMissing and "Not set" or formatLiveValue(oldValue)) or nil
					local newText = if deleting
						then "Not set"
						elseif haveNew then formatLiveValue(newValue) or "Default"
						elseif truncated then tostring(entry.value.summary or "Structured value")
						else formatPushValue(entry.value)
					local componentOldValue = haveOld and not oldValueMissing and oldValue or nil
					local componentNewValue = if deleting then nil elseif haveNew then newValue else entry.value
					local components = reviewValueComponents(componentOldValue, componentNewValue)
					local typeName = tostring(entry.dataType or entry.valueType or "")
					if typeName == "" then
						local typedValue = if haveNew then newValue elseif haveOld then oldValue else entry.value
						typeName = typeof(typedValue)
					end
					table.insert(node.props, {
						name = name,
						typeName = typeName,
						oldText = oldText,
						newText = newText,
						components = components,
						expanded = false,
					})
				end
			end
		end
		if not node.status and not node.instance and (#node.props > 0 or node.sourceEdited) then
			node.status = "added"
		end
		node.ownChanges = #node.props + ((node.status or node.sourceEdited) and 1 or 0)
	end

	local instanceCount = 0
	local function finalize(node)
		local keptCount = 0
		for _, child in ipairs(node.children) do
			if finalize(child) > 0 then
				keptCount += 1
				node.children[keptCount] = child
			end
		end
		for index = keptCount + 1, #node.children do
			node.children[index] = nil
		end
		table.sort(node.children, compareReviewNodes)
		table.sort(node.props, function(a, b)
			return a.name < b.name
		end)
		local total = node.ownChanges or 0
		if (node.ownChanges or 0) > 0 then
			instanceCount = instanceCount + 1
		end
		for _, child in ipairs(node.children) do
			total = total + child.changeTotal
		end
		node.changeTotal = total
		return node.changeTotal
	end
	local keptRoots = {}
	local effectiveCount = 0
	for _, node in ipairs(roots) do
		if node.summary or finalize(node) > 0 then
			table.insert(keptRoots, node)
			effectiveCount = effectiveCount + node.changeTotal
		end
	end

	return { roots = keptRoots, effectiveCount = effectiveCount, instanceCount = instanceCount }
end

local function flattenReviewTree(roots)
	local visible = {}
	local function visit(node, depth)
		local names = { node.displayName or node.name }
		local tail = node
		while
			#tail.children == 1
			and #tail.props == 0
			and not tail.status
			and not tail.sourceEdited
			and not tail.summary
		do
			tail = tail.children[1]
			table.insert(names, tail.displayName or tail.name)
		end
		table.insert(visible, {
			kind = "node",
			node = tail,
			depth = depth,
			chain = names,
			iconClassName = node.className,
		})
		if tail.expanded then
			for _, prop in ipairs(tail.props) do
				table.insert(visible, { kind = "prop", prop = prop, node = tail, depth = depth + 1 })
				if prop.expanded and prop.components then
					for _, component in ipairs(prop.components) do
						table.insert(visible, {
							kind = "component",
							prop = component,
							node = tail,
							depth = depth + 2,
						})
					end
				end
			end
			for _, child in ipairs(tail.children) do
				visit(child, depth + 1)
			end
		end
	end
	for _, node in ipairs(roots) do
		visit(node, 0)
	end
	return visible
end

return {
	escapeRich = escapeRich,
	colorHex = colorHex,
	classIconData = reviewClassIconData,
	buildTree = buildReviewTree,
	flattenTree = flattenReviewTree,
}
