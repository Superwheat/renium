local BridgeReferenceOverlay = {}

function BridgeReferenceOverlay.create(dependencies: { [string]: any })
	local BridgeIdentity = dependencies.BridgeIdentity
	local BridgeReferenceRetarget = dependencies.BridgeReferenceRetarget
	local CollectionService = dependencies.CollectionService
	local RbxDomModule = dependencies.RbxDomModule
	local captureExplorerSelection = dependencies.captureExplorerSelection
	local containsPackageLink = dependencies.containsPackageLink
	local pathCacheKey = dependencies.pathCacheKey
	local readProperty = dependencies.readProperty
	local removeInstanceForUndo = dependencies.removeInstanceForUndo
	local resolveOrdinalChild = dependencies.resolveOrdinalChild
	local resolvePathSegments = dependencies.resolvePathSegments
	local restoreExplorerSelection = dependencies.restoreExplorerSelection
	local setAttributeForSync = dependencies.setAttributeForSync
	local setCurrentCameraForSync = dependencies.setCurrentCameraForSync
	local setParentForSync = dependencies.setParentForSync
	local setTagForSync = dependencies.setTagForSync
	local valuesEqual = dependencies.valuesEqual
	local writePropertyForSync = dependencies.writePropertyForSync

	local ReferenceOverlay = {}

	function ReferenceOverlay.beginNativeGuard(
		prepared: { any },
		allProperties: boolean?,
		ignoredProperties: { [string]: boolean }?
	): { [string]: any }
		local guard = {
			services = {},
			connections = {},
			changedService = nil,
		}
		for _, group in ipairs(prepared) do
			guard.services[group.serviceName] = group.service
		end
		guard.connections[#guard.connections + 1] = (game :: any).ItemChanged:Connect(function(instance, propertyName)
			local normalizedProperty = string.lower(tostring(propertyName))
			if
				typeof(instance) ~= "Instance"
				or ignoredProperties ~= nil and ignoredProperties[normalizedProperty]
				or not allProperties and normalizedProperty ~= "name"
			then
				return
			end
			for serviceName, service in pairs(guard.services) do
				if instance == service or instance:IsDescendantOf(service) then
					guard.changedService = serviceName
					return
				end
			end
		end)
		for serviceName, service in pairs(guard.services) do
			local guardedServiceName = serviceName
			guard.connections[#guard.connections + 1] = service.DescendantAdded:Connect(function()
				guard.changedService = guardedServiceName
			end)
			guard.connections[#guard.connections + 1] = service.DescendantRemoving:Connect(function()
				guard.changedService = guardedServiceName
			end)
		end
		return guard
	end

	function ReferenceOverlay.assertNativeGuard(guard: { [string]: any })
		if guard.changedService ~= nil then
			error(`Studio changed {guard.changedService} while native import was staged; retry the sync`)
		end
	end

	function ReferenceOverlay.finishNativeGuard(guard: { [string]: any }?)
		if guard == nil then
			return
		end
		for _, connection in ipairs(guard.connections) do
			connection:Disconnect()
		end
		table.clear(guard.connections)
	end

	function ReferenceOverlay.capture(prepared: { any }, replacedInstances: { [Instance]: boolean }?): { any }
		local replaced = replacedInstances or {}
		if replacedInstances == nil then
			for _, group in ipairs(prepared) do
				local outgoing = group.outgoing
				if outgoing == nil then
					outgoing = {}
					for _, child in ipairs(group.target:GetChildren()) do
						if not group.preserved[child] then
							outgoing[#outgoing + 1] = child
						end
					end
				end
				for _, root in ipairs(outgoing) do
					replaced[root] = true
					for _, descendant in ipairs(root:GetDescendants()) do
						replaced[descendant] = true
					end
				end
			end
		end
		local entries = {}
		for instance in pairs(replaced) do
			for _, propertyName in ipairs(RbxDomModule.getReferencePropertyNames(instance.ClassName)) do
				local okRead, target = readProperty(instance, propertyName)
				if okRead and typeof(target) == "Instance" and not replaced[target] then
					entries[#entries + 1] = {
						instance = instance,
						propertyName = propertyName,
						target = target,
						content = false,
					}
				end
			end
			for _, propertyName in ipairs(RbxDomModule.getObjectContentPropertyNames(instance.ClassName)) do
				local okRead, value = readProperty(instance, propertyName)
				if
					okRead
					and typeof(value) == "Content"
					and value.SourceType == Enum.ContentSourceType.Object
					and value.Object ~= nil
					and not replaced[value.Object]
				then
					entries[#entries + 1] = {
						instance = instance,
						propertyName = propertyName,
						target = value.Object,
						content = true,
					}
				end
			end
		end
		return entries
	end

	function ReferenceOverlay.indexSubtree(
		byPath: { [string]: Instance },
		instance: Instance,
		pathSegments: { string },
		pathOrdinals: { number }
	): number
		byPath[pathCacheKey(pathSegments, pathOrdinals)] = instance
		local count = 1
		local nameCounts = {}
		for _, child in ipairs(instance:GetChildren()) do
			local ordinal = (nameCounts[child.Name] or 0) + 1
			nameCounts[child.Name] = ordinal
			local depth = #pathSegments + 1
			pathSegments[depth] = child.Name
			pathOrdinals[depth] = ordinal
			count += ReferenceOverlay.indexSubtree(byPath, child, pathSegments, pathOrdinals)
			pathSegments[depth] = nil
			pathOrdinals[depth] = nil
		end
		return count
	end

	local function pathExtendsTarget(pathSegments: { string }, targetPath: { string }): boolean
		if #pathSegments <= #targetPath then
			return false
		end
		for index, segment in ipairs(targetPath) do
			if pathSegments[index] ~= segment then
				return false
			end
		end
		return true
	end

	local function pathOrdinal(pathOrdinals: { number }?, index: number): number
		return if pathOrdinals ~= nil then pathOrdinals[index] or 1 else 1
	end

	local function incomingRootKey(
		targetLength: number,
		pathSegments: { string },
		pathOrdinals: { number }?
	): string
		local rootLength = targetLength + 1
		local rootSegments = table.create(rootLength)
		local rootOrdinals = table.create(rootLength)
		for index = 1, rootLength do
			rootSegments[index] = pathSegments[index]
			rootOrdinals[index] = pathOrdinal(pathOrdinals, index)
		end
		return pathCacheKey(rootSegments, rootOrdinals)
	end

	function ReferenceOverlay.resolvePreparedPath(
		prepared: { any },
		pathSegments: { string },
		pathOrdinals: { number }?,
		aliases: { [Instance]: Instance }
	): Instance?
		for _, group in ipairs(prepared) do
			if not pathExtendsTarget(pathSegments, group.targetPath) then
				continue
			end
			local targetLength = #group.targetPath
			local current = group.incomingRootsByPath[incomingRootKey(targetLength, pathSegments, pathOrdinals)]
			if current == nil then
				return nil
			end
			for index = targetLength + 2, #pathSegments do
				current = resolveOrdinalChild(current, pathSegments[index], pathOrdinal(pathOrdinals, index))
				if current == nil then
					return nil
				end
			end
			return aliases[current] or current
		end
		return nil
	end

	function ReferenceOverlay.lazyReplacements(
		prepared: { any },
		resolveStagedPath: ({ string }, { number }?) -> Instance?
	): { [Instance]: Instance }
		local missing = setmetatable({}, { __mode = "k" })
		local replacements = {}
		setmetatable(replacements, {
			__index = function(target, original)
				if typeof(original) ~= "Instance" or missing[original] then
					return nil
				end
				for _, group in ipairs(prepared) do
					local root = original
					while
						root.Parent ~= nil
						and root.Parent ~= group.target
						and not group.outgoingRootSet[root]
					do
						root = root.Parent
					end
					if group.outgoingRootSet[root] then
						local pathSegments, pathOrdinals = BridgeIdentity.getRefPathParts(original)
						if pathSegments ~= nil then
							local replacement = resolveStagedPath(pathSegments, pathOrdinals)
							if replacement ~= nil then
								rawset(target, original, replacement)
								return replacement
							end
						end
						break
					end
				end
				missing[original] = true
				return nil
			end,
		})
		return replacements
	end

	function ReferenceOverlay.chainReplacements(target: { [Instance]: Instance }, fallback: { [Instance]: Instance })
		setmetatable(target, {
			__index = function(_, original)
				return fallback[original]
			end,
		})
	end

	function ReferenceOverlay.retainedAliases(
		liveRoot: Instance,
		duplicateRoot: Instance,
		pathSegments: { string },
		pathOrdinals: { number },
		aliases: { [Instance]: Instance }
	): number
		local liveByPath = {}
		local duplicateByPath = {}
		local liveCount = ReferenceOverlay.indexSubtree(liveByPath, liveRoot, pathSegments, pathOrdinals)
		local duplicateCount = ReferenceOverlay.indexSubtree(duplicateByPath, duplicateRoot, pathSegments, pathOrdinals)
		if liveCount ~= duplicateCount then
			error(`Retained package root {table.concat(pathSegments, ".")} changed during import`)
		end
		for key, duplicate in pairs(duplicateByPath) do
			local live = liveByPath[key]
			if live == nil or live.Name ~= duplicate.Name or live.ClassName ~= duplicate.ClassName then
				error(`Retained package root {table.concat(pathSegments, ".")} changed during import`)
			end
			aliases[duplicate] = live
		end
		return duplicateCount
	end

	local function childRows(parent: Instance): { [string]: any }
		local rows = {}
		local counts = {}
		for _, child in ipairs(parent:GetChildren()) do
			local ordinal = (counts[child.Name] or 0) + 1
			counts[child.Name] = ordinal
			rows[pathCacheKey({ child.Name }, { ordinal })] = {
				instance = child,
				ordinal = ordinal,
			}
		end
		return rows
	end

	local function childPath(pathSegments, pathOrdinals, child: Instance, ordinal: number)
		local segments = table.clone(pathSegments)
		local ordinals = table.clone(pathOrdinals)
		segments[#segments + 1] = child.Name
		ordinals[#ordinals + 1] = ordinal
		return segments, ordinals
	end

	function ReferenceOverlay.preparePackageMerge(
		liveRoot: Instance,
		duplicateRoot: Instance,
		pathSegments: { string },
		pathOrdinals: { number },
		aliases: { [Instance]: Instance },
		merges: { any },
		statePairs: { any },
		packagePresence: { [Instance]: boolean }
	): number
		if liveRoot.ClassName ~= duplicateRoot.ClassName or liveRoot.Name ~= duplicateRoot.Name then
			error(`Package root {table.concat(pathSegments, ".")} changed during import`)
		end
		aliases[duplicateRoot] = liveRoot
		statePairs[#statePairs + 1] = {
			live = liveRoot,
			duplicate = duplicateRoot,
		}
		local liveRows = childRows(liveRoot)
		local matchedLive = {}
		local incoming = {}
		local preservedCount = 1
		local duplicateCounts = {}
		for _, duplicateChild in ipairs(duplicateRoot:GetChildren()) do
			local ordinal = (duplicateCounts[duplicateChild.Name] or 0) + 1
			duplicateCounts[duplicateChild.Name] = ordinal
			local key = pathCacheKey({ duplicateChild.Name }, { ordinal })
			local liveRow = liveRows[key]
			local liveChild = if liveRow ~= nil then liveRow.instance else nil
			local duplicateHasPackage = packagePresence[duplicateChild] == true
			local liveHasPackage = liveChild ~= nil and packagePresence[liveChild] == true
			if duplicateHasPackage or liveHasPackage then
				if
					liveChild == nil
					or liveChild.ClassName ~= duplicateChild.ClassName
					or liveChild:IsA("PackageLink") ~= duplicateChild:IsA("PackageLink")
					or not liveChild:IsA("PackageLink") and packagePresence[liveChild] ~= true
				then
					error(`Package root {table.concat(pathSegments, ".")} changed during import`)
				end
				matchedLive[liveChild] = true
				local segments, ordinals = childPath(pathSegments, pathOrdinals, duplicateChild, ordinal)
				if duplicateChild:IsA("PackageLink") then
					preservedCount += ReferenceOverlay.retainedAliases(
						liveChild,
						duplicateChild,
						segments,
						ordinals,
						aliases
					)
				else
					preservedCount += ReferenceOverlay.preparePackageMerge(
						liveChild,
						duplicateChild,
						segments,
						ordinals,
						aliases,
						merges,
						statePairs,
						packagePresence
					)
				end
			else
				incoming[#incoming + 1] = duplicateChild
			end
		end
		local outgoing = {}
		for _, liveChild in ipairs(liveRoot:GetChildren()) do
			if not matchedLive[liveChild] then
				if liveChild:IsA("PackageLink") then
					preservedCount += 1 + #liveChild:GetDescendants()
				else
					outgoing[#outgoing + 1] = liveChild
				end
			end
		end
		merges[#merges + 1] = {
			target = liveRoot,
			incoming = incoming,
			outgoing = outgoing,
		}
		return preservedCount
	end

	function ReferenceOverlay.copyPackageRootState(
		pair: { [string]: Instance },
		aliases: { [Instance]: Instance },
		ctx: { [string]: any },
		referencesOnly: boolean
	)
		local live = pair.live
		local duplicate = pair.duplicate
		pair.rollbackProperties = pair.rollbackProperties or {}
		pair.rollbackAttributes = pair.rollbackAttributes or {}
		local function rememberProperty(propertyName: string, value: any)
			if pair.rollbackProperties[propertyName] == nil then
				pair.rollbackProperties[propertyName] = { value = value }
			end
		end
		local function rememberAttribute(attributeName: string, value: any)
			if pair.rollbackAttributes[attributeName] == nil then
				pair.rollbackAttributes[attributeName] = { value = value }
			end
		end
		local referenceNames = {}
		for _, propertyName in ipairs(RbxDomModule.getReferencePropertyNames(duplicate.ClassName)) do
			referenceNames[propertyName] = true
		end
		for _, propertyName in ipairs(RbxDomModule.getObjectContentPropertyNames(duplicate.ClassName)) do
			referenceNames[propertyName] = true
		end
		for _, propertyName in ipairs(RbxDomModule.getWritablePropertyNames(duplicate.ClassName)) do
			if
				(referenceNames[propertyName] == true) == referencesOnly
				and propertyName ~= "Attributes"
				and propertyName ~= "Tags"
				and propertyName ~= "WorldPivot"
				and propertyName ~= "WorldPivotData"
				and propertyName ~= "Origin"
			then
				local okRead, value = readProperty(duplicate, propertyName)
				if okRead then
					if typeof(value) == "Instance" then
						value = aliases[value] or value
					elseif
						typeof(value) == "Content"
						and value.SourceType == Enum.ContentSourceType.Object
						and value.Object ~= nil
						and aliases[value.Object] ~= nil
					then
						value = Content.fromObject(aliases[value.Object])
					end
					local okCurrent, current = readProperty(live, propertyName)
					if not okCurrent or not valuesEqual(current, value) then
						if not okCurrent then
							error(`Could not read {live:GetFullName()}.{propertyName} before package merge`)
						end
						rememberProperty(propertyName, current)
						local okWrite, writeError = writePropertyForSync(live, propertyName, value, ctx)
						if not okWrite then
							error(`Could not preserve {live:GetFullName()}.{propertyName}: {writeError}`)
						end
					end
				end
			end
		end
		if referencesOnly then
			return
		end
		local desiredAttributes = duplicate:GetAttributes()
		for name in pairs(live:GetAttributes()) do
			if desiredAttributes[name] == nil then
				rememberAttribute(name, live:GetAttribute(name))
				local okWrite, writeError = setAttributeForSync(live, name, nil, ctx)
				if not okWrite then
					error(`Could not delete {live:GetFullName()} attribute {name}: {writeError}`)
				end
			end
		end
		for name, value in pairs(desiredAttributes) do
			if not valuesEqual(live:GetAttribute(name), value) then
				rememberAttribute(name, live:GetAttribute(name))
				local okWrite, writeError = setAttributeForSync(live, name, value, ctx)
				if not okWrite then
					error(`Could not write {live:GetFullName()} attribute {name}: {writeError}`)
				end
			end
		end
		local desiredTags = {}
		for _, tag in ipairs(CollectionService:GetTags(duplicate)) do
			desiredTags[tag] = true
		end
		local liveTags = CollectionService:GetTags(live)
		local tagsChanged = false
		for _, tag in ipairs(liveTags) do
			if not desiredTags[tag] then
				tagsChanged = true
				break
			end
		end
		if not tagsChanged then
			for tag in pairs(desiredTags) do
				if not CollectionService:HasTag(live, tag) then
					tagsChanged = true
					break
				end
			end
		end
		if tagsChanged and pair.rollbackTags == nil then
			pair.rollbackTags = liveTags
		end
		for _, tag in ipairs(liveTags) do
			if not desiredTags[tag] then
				setTagForSync(live, tag, false, ctx)
			end
		end
		for tag in pairs(desiredTags) do
			if not CollectionService:HasTag(live, tag) then
				setTagForSync(live, tag, true, ctx)
			end
		end
	end

	function ReferenceOverlay.restorePackageRootState(pair: { [string]: any }, ctx: { [string]: any })
		local live = pair.live
		for propertyName, entry in pairs(pair.rollbackProperties or {}) do
			local okCurrent, current = readProperty(live, propertyName)
			if not okCurrent or not valuesEqual(current, entry.value) then
				local okWrite, writeError = writePropertyForSync(live, propertyName, entry.value, ctx)
				if not okWrite then
					error(`Could not restore {live:GetFullName()}.{propertyName}: {writeError}`)
				end
			end
		end
		for attributeName, entry in pairs(pair.rollbackAttributes or {}) do
			if not valuesEqual(live:GetAttribute(attributeName), entry.value) then
				local okWrite, writeError = setAttributeForSync(live, attributeName, entry.value, ctx)
				if not okWrite then
					error(`Could not restore {live:GetFullName()} attribute {attributeName}: {writeError}`)
				end
			end
		end
		if pair.rollbackTags ~= nil then
			local desired = {}
			for _, tag in ipairs(pair.rollbackTags) do
				desired[tag] = true
			end
			for _, tag in ipairs(CollectionService:GetTags(live)) do
				if not desired[tag] then
					setTagForSync(live, tag, false, ctx)
				end
				desired[tag] = nil
			end
			for tag in pairs(desired) do
				setTagForSync(live, tag, true, ctx)
			end
		end
	end

	function ReferenceOverlay.assertPackageRoots(group: { [string]: any })
		local actual = {}
		for _, root in ipairs(group.packageScanRoots) do
			if root.Parent == group.target and containsPackageLink(root) then
				actual[root] = true
			end
		end
		local expected = 0
		for _, descriptor in ipairs(group.packageRoots) do
			local root = resolvePathSegments(descriptor.pathSegments, nil, descriptor.pathOrdinals)
			if
				root == nil
				or root.Parent ~= group.target
				or root.ClassName ~= descriptor.className
				or not actual[root]
			then
				error(`Package root {table.concat(descriptor.pathSegments, ".")} changed during import`)
			end
			actual[root] = nil
			expected += 1
		end
		if next(actual) then
			error("Studio package roots changed during import")
		end
		return expected
	end

	function ReferenceOverlay.assertNativeImportState(undo: { [string]: any }, ctx: { [string]: any })
		ReferenceOverlay.assertNativeGuard(undo.guard)
		for serviceName, generation in pairs(undo.generationsByService) do
			if ctx.studioChangeGeneration(serviceName) ~= generation then
				error(`Studio changed {serviceName} while native import was staged; retry the sync`)
			end
		end
		for _, group in ipairs(undo.prepared) do
			ReferenceOverlay.assertPackageRoots(group)
		end
	end

	function ReferenceOverlay.apply(entries: { any }, replacements: { [Instance]: Instance }, ctx: { [string]: any }?)
		for _, entry in ipairs(entries) do
			local instance = replacements[entry.instance]
			if instance ~= nil then
				local target = replacements[entry.target] or entry.target
				local value = if entry.content then Content.fromObject(target) else target
				local okWrite, writeError = writePropertyForSync(instance, entry.propertyName, value, ctx)
				if not okWrite then
					error(`Could not restore {instance:GetFullName()}.{entry.propertyName}: {writeError}`)
				end
			end
		end
	end

	function ReferenceOverlay.retargetPreservedContent(
		roots: { Instance },
		replacements: { [Instance]: Instance },
		ctx: { [string]: any }?,
		excludedRoots: { [Instance]: boolean }?
	): (number, number)
		local updated, failed = BridgeReferenceRetarget.apply(
			roots,
			replacements,
			RbxDomModule.getObjectContentPropertyNames,
			function(instance, propertyName)
				local okRead, value = readProperty(instance, propertyName)
				if not okRead or typeof(value) ~= "Content" or value.SourceType ~= Enum.ContentSourceType.Object then
					return false, nil
				end
				return true, value.Object
			end,
			function(instance, propertyName, replacement)
				return writePropertyForSync(instance, propertyName, Content.fromObject(replacement), ctx)
			end,
			excludedRoots
		)
		return updated, failed
	end

	local function indexPackagePresence(root: Instance, packagePresence: { [Instance]: boolean }): boolean
		local hasPackage = root:IsA("PackageLink")
		for _, child in ipairs(root:GetChildren()) do
			if indexPackagePresence(child, packagePresence) then
				hasPackage = true
			end
		end
		packagePresence[root] = hasPackage
		return hasPackage
	end

	local function initializePreparedGroup(group: { [string]: any }, packagePresence: { [Instance]: boolean })
		for _, roots in ipairs({ group.outgoing, group.incoming }) do
			for _, root in ipairs(roots) do
				indexPackagePresence(root, packagePresence)
			end
		end
		group.packageScanRoots = table.clone(group.outgoing)
		group.incomingRootsByPath = {}
		group.retainedLiveRoots = {}
		for index, descriptor in ipairs(group.rootPaths) do
			local incomingRoot = group.incomingByPayloadIndex[index]
			if incomingRoot ~= nil then
				group.incomingRootsByPath[pathCacheKey(descriptor.pathSegments, descriptor.pathOrdinals)] = incomingRoot
			end
		end
		group.outgoingRootSet = {}
		for _, root in ipairs(group.outgoing) do
			group.outgoingRootSet[root] = true
		end
	end

	local function retainedRootChanged(pathSegments: { string })
		error(`Retained package root {table.concat(pathSegments, ".")} changed during import`)
	end

	local function prepareRetainedRoot(
		group: { [string]: any },
		descriptor: { [string]: any },
		state: { [string]: any }
	)
		local duplicate = group.incomingByPayloadIndex[descriptor.payloadIndex]
		local live = resolvePathSegments(descriptor.pathSegments, nil, descriptor.pathOrdinals)
		if
			duplicate == nil
			or live == nil
			or live.Parent ~= group.target
			or not group.outgoingRootSet[live]
			or live.ClassName ~= descriptor.className
			or duplicate.ClassName ~= descriptor.className
			or live.Name ~= descriptor.pathSegments[#descriptor.pathSegments]
			or duplicate.Name ~= live.Name
			or state.packagePresence[live] ~= true
			or descriptor.payloadOmitted and #duplicate:GetChildren() ~= 0
		then
			retainedRootChanged(descriptor.pathSegments)
		end

		local retainedInstanceCount
		if descriptor.payloadOmitted or duplicate:IsA("PackageLink") then
			retainedInstanceCount = descriptor.instanceCount
		else
			retainedInstanceCount = ReferenceOverlay.preparePackageMerge(
				live,
				duplicate,
				descriptor.pathSegments,
				descriptor.pathOrdinals,
				state.incomingAliases,
				state.packageMerges,
				state.packageStatePairs,
				state.packagePresence
			)
		end

		if descriptor.payloadOmitted then
			if 1 + #live:GetDescendants() ~= descriptor.instanceCount then
				retainedRootChanged(descriptor.pathSegments)
			end
			state.incomingAliases[duplicate] = live
			group.incomingRootsByPath[pathCacheKey(descriptor.pathSegments, descriptor.pathOrdinals)] = live
		elseif duplicate:IsA("PackageLink") then
			local duplicateInstanceCount = ReferenceOverlay.retainedAliases(
				live,
				duplicate,
				descriptor.pathSegments,
				descriptor.pathOrdinals,
				state.incomingAliases
			)
			if duplicateInstanceCount ~= descriptor.instanceCount then
				retainedRootChanged(descriptor.pathSegments)
			end
		end

		state.excludedIncoming[duplicate] = true
		state.excludedOutgoing[live] = true
		group.retainedLiveRoots[#group.retainedLiveRoots + 1] = live
		state.retainedDuplicates[#state.retainedDuplicates + 1] = duplicate
		state.retainedDuplicateInstanceCount += retainedInstanceCount
	end

	local function filterRoots(
		roots: { Instance },
		excluded: { [Instance]: boolean },
		scanRoots: { Instance }
	): { Instance }
		local included = {}
		for _, root in ipairs(roots) do
			if not excluded[root] then
				included[#included + 1] = root
				scanRoots[#scanRoots + 1] = root
			end
		end
		return included
	end

	local function collectScanRoots(prepared: { any }, state: { [string]: any }): ({ Instance }, { Instance })
		for duplicate, live in pairs(state.incomingAliases) do
			state.excludedIncoming[duplicate] = true
			state.excludedOutgoing[live] = true
		end
		local incomingScanRoots = {}
		local outgoingScanRoots = {}
		for _, group in ipairs(prepared) do
			group.incoming = filterRoots(group.incoming, state.excludedIncoming, incomingScanRoots)
			group.outgoing = filterRoots(group.outgoing, state.excludedOutgoing, outgoingScanRoots)
		end
		for _, merge in ipairs(state.packageMerges) do
			for _, root in ipairs(merge.incoming) do
				incomingScanRoots[#incomingScanRoots + 1] = root
			end
			for _, root in ipairs(merge.outgoing) do
				outgoingScanRoots[#outgoingScanRoots + 1] = root
				for _, group in ipairs(prepared) do
					if root:IsDescendantOf(group.target) then
						group.outgoingRootSet[root] = true
						break
					end
				end
			end
		end
		return incomingScanRoots, outgoingScanRoots
	end

	local function retargetIncomingAliases(
		incomingScanRoots: { Instance },
		incomingAliases: { [Instance]: Instance },
		ctx: { [string]: any }
	): (number, number)
		if next(incomingAliases) == nil then
			return 0, 0
		end
		local aliasUpdated, aliasFailed, aliasFailures = BridgeReferenceRetarget.apply(
			incomingScanRoots,
			incomingAliases,
			RbxDomModule.getReferencePropertyNames,
			readProperty,
			function(instance, propertyName, value)
				return writePropertyForSync(instance, propertyName, value, ctx)
			end
		)
		if aliasFailed > 0 then
			local first = aliasFailures[1]
			error(
				`Could not retain {aliasFailed} package references; first failure: {first.instance:GetFullName()}.{first.propertyName}: {first.error}`
			)
		end
		local aliasContentUpdated, aliasContentFailed =
			ReferenceOverlay.retargetPreservedContent(incomingScanRoots, incomingAliases, ctx)
		if aliasContentFailed > 0 then
			error(`Could not retain {aliasContentFailed} package content references`)
		end
		return aliasUpdated, aliasContentUpdated
	end

	local function captureOutgoingReferences(
		prepared: { any },
		outgoingScanRoots: { Instance },
		externalReferencesPostApplied: boolean
	): { any }
		if externalReferencesPostApplied then
			return {}
		end
		local replaced = {}
		for _, root in ipairs(outgoingScanRoots) do
			replaced[root] = true
			for _, descendant in ipairs(root:GetDescendants()) do
				replaced[descendant] = true
			end
		end
		return ReferenceOverlay.capture(prepared, replaced)
	end

	local function countRemovedRoots(prepared: { any }, packageMerges: { any }): number
		local count = 0
		for _, group in ipairs(prepared) do
			count += #group.outgoing
		end
		for _, merge in ipairs(packageMerges) do
			count += #merge.outgoing
		end
		return count
	end

	function ReferenceOverlay.prepareRetained(
		prepared: { any },
		ctx: { [string]: any },
		externalReferencesPostApplied: boolean
	): { [string]: any }
		local state = {
			excludedIncoming = {},
			excludedOutgoing = {},
			incomingAliases = {},
			retainedDuplicates = {},
			retainedDuplicateInstanceCount = 0,
			packageMerges = {},
			packageStatePairs = {},
			packagePresence = {},
		}
		for _, group in ipairs(prepared) do
			initializePreparedGroup(group, state.packagePresence)
			for _, descriptor in ipairs(group.retainedRoots) do
				prepareRetainedRoot(group, descriptor, state)
			end
		end
		local incomingScanRoots, outgoingScanRoots = collectScanRoots(prepared, state)
		local aliasUpdated, aliasContentUpdated = retargetIncomingAliases(incomingScanRoots, state.incomingAliases, ctx)
		local referenceOverlay =
			captureOutgoingReferences(prepared, outgoingScanRoots, externalReferencesPostApplied)
		local function resolveStagedPath(pathSegments, pathOrdinals)
			return ReferenceOverlay.resolvePreparedPath(prepared, pathSegments, pathOrdinals, state.incomingAliases)
		end
		local replacements = ReferenceOverlay.lazyReplacements(prepared, resolveStagedPath)
		return {
			referenceOverlay = referenceOverlay,
			replacements = replacements,
			needsReferenceRetarget = #incomingScanRoots > 0 and #outgoingScanRoots > 0,
			resolveStagedPath = resolveStagedPath,
			retainedDuplicates = state.retainedDuplicates,
			retainedDuplicateInstanceCount = state.retainedDuplicateInstanceCount,
			packageAliases = state.incomingAliases,
			packageMerges = state.packageMerges,
			packageStatePairs = state.packageStatePairs,
			removedRootCount = countRemovedRoots(prepared, state.packageMerges),
			referenceUpdates = #referenceOverlay + aliasUpdated + aliasContentUpdated,
		}
	end

	function ReferenceOverlay.commitNative(undo: { [string]: any }, ctx: { [string]: any }): (number, number)
		ReferenceOverlay.assertNativeImportState(undo, ctx)
		ReferenceOverlay.finishNativeGuard(undo.guard)
		undo.guard = nil
		local selected = captureExplorerSelection()
		local selectionPaths = {}
		for _, instance in ipairs(selected) do
			local pathSegments, pathOrdinals = BridgeIdentity.getRefPathParts(instance)
			if pathSegments ~= nil then
				selectionPaths[instance] = {
					pathSegments = pathSegments,
					pathOrdinals = pathOrdinals,
				}
			end
		end
		local excludedRoots = {}
		for _, group in ipairs(undo.prepared) do
			for _, instance in ipairs(group.outgoing) do
				excludedRoots[instance] = true
			end
			for _, instance in ipairs(group.incoming) do
				excludedRoots[instance] = true
			end
			for _, instance in ipairs(group.retainedLiveRoots) do
				excludedRoots[instance] = true
			end
		end
		for _, merge in ipairs(undo.packageMerges or {}) do
			for _, instance in ipairs(merge.outgoing) do
				excludedRoots[instance] = true
			end
			for _, instance in ipairs(merge.incoming) do
				excludedRoots[instance] = true
			end
		end
		local updated = 0
		local contentUpdated = 0
		if undo.needsReferenceRetarget then
			local scanRoots = {}
			for serviceName, allowed in pairs(ctx.allowedServices) do
				if allowed then
					scanRoots[#scanRoots + 1] = game:GetService(serviceName)
				end
			end
			local failed, failures
			updated, failed, failures = BridgeReferenceRetarget.apply(
				scanRoots,
				undo.replacements,
				RbxDomModule.getReferencePropertyNames,
				readProperty,
				function(instance, propertyName, value)
					return writePropertyForSync(instance, propertyName, value, ctx)
				end,
				excludedRoots
			)
			if failed > 0 then
				local first = failures[1]
				error(
					`Could not retarget {failed} native import references; first failure: {first.instance:GetFullName()}.{first.propertyName}: {first.error}`
				)
			end
			local contentFailed
			contentUpdated, contentFailed =
				ReferenceOverlay.retargetPreservedContent(scanRoots, undo.replacements, ctx, excludedRoots)
			if contentFailed > 0 then
				error(`Could not retarget {contentFailed} native import content references`)
			end
		end
		for _, pair in ipairs(undo.packageStatePairs or {}) do
			ReferenceOverlay.copyPackageRootState(pair, undo.packageAliases, ctx, false)
		end
		for _, group in ipairs(undo.prepared) do
			for _, instance in ipairs(group.incoming) do
				setParentForSync(instance, group.target, ctx)
			end
		end
		for _, merge in ipairs(undo.packageMerges or {}) do
			for _, instance in ipairs(merge.incoming) do
				setParentForSync(instance, merge.target, ctx)
			end
		end
		for _, pair in ipairs(undo.packageStatePairs or {}) do
			ReferenceOverlay.copyPackageRootState(pair, undo.packageAliases, ctx, true)
		end
		local removedRootCount = 0
		for _, merge in ipairs(undo.packageMerges or {}) do
			for _, instance in ipairs(merge.outgoing) do
				removeInstanceForUndo(instance, ctx)
				removedRootCount += 1
			end
		end
		for _, group in ipairs(undo.prepared) do
			for _, instance in ipairs(group.outgoing) do
				removeInstanceForUndo(instance, ctx)
				removedRootCount += 1
			end
		end
		local selectionReplacements = {}
		for instance, path in pairs(selectionPaths) do
			if instance.Parent == nil then
				local replacement = undo.resolveStagedPath(path.pathSegments, path.pathOrdinals)
				if replacement ~= nil then
					selectionReplacements[instance] = replacement
				end
			end
		end
		restoreExplorerSelection(selected, selectionReplacements)
		for _, group in ipairs(undo.prepared) do
			ctx.invalidateService(group.serviceName)
		end
		return removedRootCount, updated + contentUpdated + undo.referenceUpdates
	end

	local function restoreRollbackRoots(
		incomingRoots: { Instance },
		outgoingRoots: { Instance },
		target: Instance,
		incoming: { Instance },
		restoreParent: (Instance, Instance?) -> ()
	)
		for _, instance in ipairs(incomingRoots) do
			incoming[#incoming + 1] = instance
			if instance.Parent ~= nil then
				restoreParent(instance, nil)
			end
		end
		for _, instance in ipairs(outgoingRoots) do
			if instance.Parent == nil then
				restoreParent(instance, target)
			end
		end
	end

	local function assertRollbackRoots(
		incomingRoots: { Instance },
		outgoingRoots: { Instance },
		target: Instance,
		incomingLabel: string,
		outgoingLabel: string
	)
		for _, instance in ipairs(outgoingRoots) do
			if instance.Parent ~= target then
				error(outgoingLabel .. instance.Name)
			end
		end
		for _, instance in ipairs(incomingRoots) do
			if instance.Parent ~= nil then
				error(incomingLabel .. instance.Name .. " remained live after rollback")
			end
		end
	end

	local function restorePackageStates(
		pairs: { any },
		ctx: { [string]: any },
		structuralErrors: { string }
	)
		for _, pair in ipairs(pairs) do
			local restored, result = pcall(ReferenceOverlay.restorePackageRootState, pair, ctx)
			if not restored then
				structuralErrors[#structuralErrors + 1] = tostring(result)
			end
		end
	end

	local function reverseReplacementMap(replacements: { [Instance]: Instance }): { [Instance]: Instance }
		local reversed = {}
		for original, replacement in pairs(replacements) do
			reversed[replacement] = original
		end
		return reversed
	end

	local function allowedServiceRoots(allowedServices: { [string]: boolean }): { Instance }
		local roots = {}
		for serviceName, allowed in pairs(allowedServices) do
			if allowed then
				roots[#roots + 1] = game:GetService(serviceName)
			end
		end
		return roots
	end

	local function rollbackReferences(
		replacements: { [Instance]: Instance },
		ctx: { [string]: any }
	)
		local scanRoots = allowedServiceRoots(ctx.allowedServices)
		local _, failed = BridgeReferenceRetarget.apply(
			scanRoots,
			replacements,
			RbxDomModule.getReferencePropertyNames,
			readProperty,
			function(instance, propertyName, value)
				return writePropertyForSync(instance, propertyName, value, ctx)
			end
		)
		if failed > 0 then
			error(`Could not roll back {failed} native import references`)
		end
		local _, contentFailed = ReferenceOverlay.retargetPreservedContent(scanRoots, replacements, ctx)
		if contentFailed > 0 then
			error(`Could not roll back {contentFailed} native import content references`)
		end
	end

	function ReferenceOverlay.rollbackNative(undo: { [string]: any }, ctx: { [string]: any }): { Instance }
		ReferenceOverlay.finishNativeGuard(undo.guard)
		undo.guard = nil
		local incoming = {}
		local structuralErrors = {}
		local function restoreParent(instance: Instance, parent: Instance?)
			local ok, result = pcall(setParentForSync, instance, parent, ctx)
			if not ok then
				structuralErrors[#structuralErrors + 1] = tostring(result)
			end
		end
		for _, merge in ipairs(undo.packageMerges or {}) do
			restoreRollbackRoots(merge.incoming, merge.outgoing, merge.target, incoming, restoreParent)
		end
		for _, group in ipairs(undo.prepared) do
			restoreRollbackRoots(group.incoming, group.outgoing, group.target, incoming, restoreParent)
		end
		for _, instance in ipairs(undo.retainedDuplicates or {}) do
			incoming[#incoming + 1] = instance
			if instance.Parent ~= nil then
				restoreParent(instance, nil)
			end
		end
		if #structuralErrors > 0 then
			error(`Could not roll back {#structuralErrors} native import roots: {structuralErrors[1]}`)
		end
		for _, merge in ipairs(undo.packageMerges or {}) do
			assertRollbackRoots(
				merge.incoming,
				merge.outgoing,
				merge.target,
				"Incoming package content ",
				"Could not roll back package content "
			)
		end
		for _, group in ipairs(undo.prepared) do
			assertRollbackRoots(
				group.incoming,
				group.outgoing,
				group.target,
				"Incoming native import root ",
				"Could not roll back native import root "
			)
			for _, instance in ipairs(group.retainedLiveRoots or {}) do
				if instance.Parent ~= group.target then
					error(`Retained package root {instance.Name} was lost during rollback`)
				end
			end
		end
		restorePackageStates(undo.packageStatePairs or {}, ctx, structuralErrors)
		if #structuralErrors > 0 then
			error(`Could not restore retained package state: {structuralErrors[1]}`)
		end
		local reverseReplacements = reverseReplacementMap(undo.replacements)
		rollbackReferences(reverseReplacements, ctx)
		if undo.currentCamera ~= nil then
			setCurrentCameraForSync(reverseReplacements[undo.currentCamera] or undo.currentCamera, ctx)
		end
		return incoming
	end

	return ReferenceOverlay
end

return BridgeReferenceOverlay
