local BridgeReferenceOverlay = {}

function BridgeReferenceOverlay.create(dependencies: { [string]: any })
	local BridgeIdentity = dependencies.BridgeIdentity
	local BridgeReferenceRetarget = dependencies.BridgeReferenceRetarget
	local RbxDomModule = dependencies.RbxDomModule
	local captureExplorerSelection = dependencies.captureExplorerSelection
	local containsPackageLink = dependencies.containsPackageLink
	local pathCacheKey = dependencies.pathCacheKey
	local readProperty = dependencies.readProperty
	local removeInstanceForUndo = dependencies.removeInstanceForUndo
	local resolveOrdinalChild = dependencies.resolveOrdinalChild
	local resolvePathSegments = dependencies.resolvePathSegments
	local restoreExplorerSelection = dependencies.restoreExplorerSelection
	local setCurrentCameraForSync = dependencies.setCurrentCameraForSync
	local setParentForSync = dependencies.setParentForSync
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

	function ReferenceOverlay.resolvePreparedPath(
		prepared: { any },
		pathSegments: { string },
		pathOrdinals: { number }?,
		aliases: { [Instance]: Instance }
	): Instance?
		for _, group in ipairs(prepared) do
			local targetPath = group.targetPath
			local targetLength = #targetPath
			if #pathSegments <= targetLength then
				continue
			end
			local matches = true
			for index = 1, targetLength do
				if pathSegments[index] ~= targetPath[index] then
					matches = false
					break
				end
			end
			if not matches then
				continue
			end
			local rootSegments = table.create(targetLength + 1)
			local rootOrdinals = table.create(targetLength + 1)
			for index = 1, targetLength + 1 do
				rootSegments[index] = pathSegments[index]
				rootOrdinals[index] = if pathOrdinals ~= nil then pathOrdinals[index] or 1 else 1
			end
			local current = group.incomingRootsByPath[pathCacheKey(rootSegments, rootOrdinals)]
			if current == nil then
				return nil
			end
			for index = targetLength + 2, #pathSegments do
				current = resolveOrdinalChild(
					current,
					pathSegments[index],
					if pathOrdinals ~= nil then pathOrdinals[index] or 1 else 1
				)
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
					while root.Parent ~= nil and root.Parent ~= group.target do
						root = root.Parent
					end
					if root.Parent == group.target and group.outgoingRootSet[root] then
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

	function ReferenceOverlay.stripIncomingPackages(root: Instance): number
		if root:IsA("PackageLink") then
			error("A native import root cannot be a PackageLink")
		end
		local removed = 0
		for _, child in ipairs(root:GetChildren()) do
			if child:IsA("PackageLink") then
				removed += 1 + #child:GetDescendants()
				child:Destroy()
			end
		end
		if removed == 0 then
			error(`Package root {root.Name} no longer contains a PackageLink`)
		end
		return removed
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

	function ReferenceOverlay.prepareRetained(
		prepared: { any },
		ctx: { [string]: any },
		externalReferencesPostApplied: boolean
	): { [string]: any }
		local excludedIncoming = {}
		local excludedOutgoing = {}
		local incomingAliases = {}
		local retainedDuplicates = {}
		local retainedDuplicateInstanceCount = 0
		for _, group in ipairs(prepared) do
			group.packageScanRoots = table.clone(group.outgoing)
			group.incomingRootsByPath = {}
			group.retainedLiveRoots = {}
			for index, descriptor in ipairs(group.rootPaths) do
				local incomingRoot = group.incomingByPayloadIndex[index]
				if incomingRoot ~= nil then
					group.incomingRootsByPath[pathCacheKey(descriptor.pathSegments, descriptor.pathOrdinals)] =
						incomingRoot
				end
			end
			group.outgoingRootSet = {}
			local outgoingInGroup = {}
			for _, root in ipairs(group.outgoing) do
				outgoingInGroup[root] = true
				group.outgoingRootSet[root] = true
			end
			for _, descriptor in ipairs(group.retainedRoots) do
				local duplicate = group.incomingByPayloadIndex[descriptor.payloadIndex]
				local live = resolvePathSegments(descriptor.pathSegments, nil, descriptor.pathOrdinals)
				if
					duplicate == nil
					or live == nil
					or live.Parent ~= group.target
					or not outgoingInGroup[live]
					or live.ClassName ~= descriptor.className
					or duplicate.ClassName ~= descriptor.className
					or live.Name ~= descriptor.pathSegments[#descriptor.pathSegments]
					or duplicate.Name ~= live.Name
					or (not live:IsA("PackageLink") and live:FindFirstChildWhichIsA("PackageLink", true) == nil)
					or descriptor.payloadOmitted and #duplicate:GetChildren() ~= 0
					or not descriptor.payloadOmitted
						and (
						not duplicate:IsA("PackageLink")
						and duplicate:FindFirstChildWhichIsA("PackageLink", true) == nil
					)
				then
					error(`Retained package root {table.concat(descriptor.pathSegments, ".")} changed during import`)
				end
				if not descriptor.payloadOmitted then
					local duplicateInstanceCount = ReferenceOverlay.retainedAliases(
						live,
						duplicate,
						descriptor.pathSegments,
						descriptor.pathOrdinals,
						incomingAliases
					)
					if duplicateInstanceCount ~= descriptor.instanceCount then
						error(`Retained package root {table.concat(descriptor.pathSegments, ".")} changed during import`)
					end
				end
				excludedIncoming[duplicate] = true
				excludedOutgoing[live] = true
				group.retainedLiveRoots[#group.retainedLiveRoots + 1] = live
				retainedDuplicates[#retainedDuplicates + 1] = duplicate
				retainedDuplicateInstanceCount += descriptor.instanceCount
			end
		end
		for duplicate, live in pairs(incomingAliases) do
			excludedIncoming[duplicate] = true
			excludedOutgoing[live] = true
		end
		local incomingScanRoots = {}
		local outgoingScanRoots = {}
		for _, group in ipairs(prepared) do
			local incoming = {}
			for _, root in ipairs(group.incoming) do
				if not excludedIncoming[root] then
					incoming[#incoming + 1] = root
					incomingScanRoots[#incomingScanRoots + 1] = root
				end
			end
			group.incoming = incoming
			local outgoing = {}
			for _, root in ipairs(group.outgoing) do
				if not excludedOutgoing[root] then
					outgoing[#outgoing + 1] = root
					outgoingScanRoots[#outgoingScanRoots + 1] = root
				end
			end
			group.outgoing = outgoing
		end
		local aliasUpdated = 0
		local aliasContentUpdated = 0
		if next(incomingAliases) then
			local aliasUpdatedResult, aliasFailed, aliasFailures = BridgeReferenceRetarget.apply(
				incomingScanRoots,
				incomingAliases,
				RbxDomModule.getReferencePropertyNames,
				readProperty,
				function(instance, propertyName, value)
					return writePropertyForSync(instance, propertyName, value, ctx)
				end
			)
			aliasUpdated = aliasUpdatedResult
			if aliasFailed > 0 then
				local first = aliasFailures[1]
				error(
					`Could not retain {aliasFailed} package references; first failure: {first.instance:GetFullName()}.{first.propertyName}: {first.error}`
				)
			end
			local aliasContentFailed
			aliasContentUpdated, aliasContentFailed =
				ReferenceOverlay.retargetPreservedContent(incomingScanRoots, incomingAliases, ctx)
			if aliasContentFailed > 0 then
				error(`Could not retain {aliasContentFailed} package content references`)
			end
		end
		local referenceOverlay = {}
		if not externalReferencesPostApplied then
			local replaced = {}
			for _, root in ipairs(outgoingScanRoots) do
				replaced[root] = true
				for _, descendant in ipairs(root:GetDescendants()) do
					replaced[descendant] = true
				end
			end
			referenceOverlay = ReferenceOverlay.capture(prepared, replaced)
		end
		local function resolveStagedPath(pathSegments, pathOrdinals)
			return ReferenceOverlay.resolvePreparedPath(prepared, pathSegments, pathOrdinals, incomingAliases)
		end
		local replacements = ReferenceOverlay.lazyReplacements(prepared, resolveStagedPath)
		local removedRootCount = 0
		for _, group in ipairs(prepared) do
			removedRootCount += #group.outgoing
		end
		return {
			referenceOverlay = referenceOverlay,
			replacements = replacements,
			resolveStagedPath = resolveStagedPath,
			retainedDuplicates = retainedDuplicates,
			retainedDuplicateInstanceCount = retainedDuplicateInstanceCount,
			removedRootCount = removedRootCount,
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
		local scanRoots = {}
		for serviceName, allowed in pairs(ctx.allowedServices) do
			if allowed then
				scanRoots[#scanRoots + 1] = game:GetService(serviceName)
			end
		end
		local updated, failed, failures = BridgeReferenceRetarget.apply(
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
		local contentUpdated, contentFailed =
			ReferenceOverlay.retargetPreservedContent(scanRoots, undo.replacements, ctx, excludedRoots)
		if contentFailed > 0 then
			error(`Could not retarget {contentFailed} native import content references`)
		end
		for _, group in ipairs(undo.prepared) do
			for _, instance in ipairs(group.incoming) do
				setParentForSync(instance, group.target, ctx)
			end
		end
		local removedRootCount = 0
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
		for _, group in ipairs(undo.prepared) do
			for _, instance in ipairs(group.incoming) do
				incoming[#incoming + 1] = instance
				if instance.Parent ~= nil then
					restoreParent(instance, nil)
				end
			end
			for _, instance in ipairs(group.outgoing) do
				if instance.Parent == nil then
					restoreParent(instance, group.target)
				end
			end
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
		for _, group in ipairs(undo.prepared) do
			for _, instance in ipairs(group.outgoing) do
				if instance.Parent ~= group.target then
					error(`Could not roll back native import root {instance.Name}`)
				end
			end
			for _, instance in ipairs(group.retainedLiveRoots or {}) do
				if instance.Parent ~= group.target then
					error(`Retained package root {instance.Name} was lost during rollback`)
				end
			end
			for _, instance in ipairs(group.incoming) do
				if instance.Parent ~= nil then
					error(`Incoming native import root {instance.Name} remained live after rollback`)
				end
			end
		end
		local reverseReplacements = {}
		for original, replacement in pairs(undo.replacements) do
			reverseReplacements[replacement] = original
		end
		local scanRoots = {}
		for serviceName, allowed in pairs(ctx.allowedServices) do
			if allowed then
				scanRoots[#scanRoots + 1] = game:GetService(serviceName)
			end
		end
		local _, failed = BridgeReferenceRetarget.apply(
			scanRoots,
			reverseReplacements,
			RbxDomModule.getReferencePropertyNames,
			readProperty,
			function(instance, propertyName, value)
				return writePropertyForSync(instance, propertyName, value, ctx)
			end
		)
		if failed > 0 then
			error(`Could not roll back {failed} native import references`)
		end
		local _, contentFailed = ReferenceOverlay.retargetPreservedContent(scanRoots, reverseReplacements, ctx)
		if contentFailed > 0 then
			error(`Could not roll back {contentFailed} native import content references`)
		end
		if undo.currentCamera ~= nil then
			setCurrentCameraForSync(reverseReplacements[undo.currentCamera] or undo.currentCamera, ctx)
		end
		return incoming
	end

	return ReferenceOverlay
end

return BridgeReferenceOverlay
