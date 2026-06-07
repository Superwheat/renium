--!nocheck

local BridgeProfiling = {}

function BridgeProfiling.install(Config, context)
	local HttpService = context.httpService
	local IdentityModule = context.identityModule
	local COMPACT_TYPE_IDS = context.compactTypeIds

	function Config.parseProfileFlags(raw)
		local flags = {}
		if type(raw) ~= "table" then
			return flags
		end
		for key, value in pairs(raw) do
			if type(key) == "number" then
				if type(value) == "string" and value ~= "" then
					if value == "all" then
						return {}
					end
					flags[value] = true
					if value == "read" then
						flags.instance = true
					elseif value == "instance" then
						flags.read = true
					end
				end
			elseif value == true then
				flags[tostring(key)] = true
			end
		end
		return flags
	end

	function Config.profileFlagEnabled(flags, name)
		return next(flags) == nil or flags[name] == true
	end

	function Config.copySorted(values)
		local out = table.create(#values)
		for i, value in ipairs(values) do
			out[i] = value
		end
		table.sort(out)
		return out
	end

	function Config.percentile(sorted, percentile)
		if #sorted == 0 then
			return 0
		end
		local index = math.clamp(math.ceil(#sorted * percentile), 1, #sorted)
		return sorted[index]
	end

	function Config.profileTimedOperation(iterations, body, emptyBody)
		local times = table.create(iterations)
		local emptyTimes = table.create(iterations)
		local totalUs = 0
		local totalCalls = 0
		local emptyTotalUs = 0
		local warmupIterations = math.min(2, iterations)
		for i = 1, warmupIterations do
			if emptyBody ~= nil then
				emptyBody(i)
			end
			body(i)
		end
		for i = 1, iterations do
			if emptyBody ~= nil then
				local started = os.clock()
				emptyBody(i)
				local elapsed = (os.clock() - started) * 1000000
				emptyTimes[i] = elapsed
				emptyTotalUs += elapsed
			else
				emptyTimes[i] = 0
			end
		end
		for i = 1, iterations do
			local started = os.clock()
			local calls = body(i) or 0
			local elapsed = (os.clock() - started) * 1000000 - (emptyTimes[i] or 0)
			if elapsed < 0 then
				elapsed = 0
			end
			times[i] = elapsed
			totalUs += elapsed
			totalCalls += calls
		end
		local sorted = Config.copySorted(times)
		return {
			iterations = iterations,
			calls = totalCalls,
			totalUs = totalUs,
			avgUs = iterations > 0 and totalUs / iterations or 0,
			perCallUs = totalCalls > 0 and totalUs / totalCalls or 0,
			p50Us = Config.percentile(sorted, 0.5),
			p90Us = Config.percentile(sorted, 0.9),
			emptyAvgUs = iterations > 0 and emptyTotalUs / iterations or 0,
		}
	end

	function Config.profileSampleOperation(samples, iterations, callback, emptyCallback)
		return Config.profileTimedOperation(iterations, function()
			for i, sample in ipairs(samples) do
				callback(sample, i)
			end
			return #samples
		end, if emptyCallback ~= nil then function()
			for i, sample in ipairs(samples) do
				emptyCallback(sample, i)
			end
			return #samples
		end else function()
			for i = 1, #samples do
				local _ = samples[i]
			end
			return #samples
		end)
	end

	function Config.profileFixedCountOperation(callCount, iterations, callback, emptyCallback)
		return Config.profileTimedOperation(iterations, function()
			for i = 1, callCount do
				callback(i)
			end
			return callCount
		end, if emptyCallback ~= nil then function()
			for i = 1, callCount do
				emptyCallback(i)
			end
			return callCount
		end else function()
			for i = 1, callCount do
				local _ = i
			end
			return callCount
		end)
	end

	function Config.ensureProfileState(serviceName)
		return context.getState(serviceName)
	end

	function Config.appendUniqueProfileSample(out, seen, state, index)
		local instances = state.instances
		if index < 1 or index > #instances or seen[index] == true then
			return
		end
		seen[index] = true
		local instance = instances[index]
		out[#out + 1] = {
			index = index,
			instance = instance,
			className = state.classNameByIndex[index] or instance.ClassName,
			name = state.nameByIndex[index] or instance.Name,
		}
	end

	function Config.buildProfileSamples(state, sampleCount)
		local total = #state.instances
		local perGroup = math.max(1, math.floor(math.max(4, sampleCount) / 4))
		local seen = {}
		local first = {}
		local middle = {}
		local last = {}
		local mixed = {}
		local combined = {}
		for i = 1, math.min(perGroup, total) do
			Config.appendUniqueProfileSample(first, seen, state, i)
		end
		local middleStart = math.max(1, math.floor(total / 2) - math.floor(perGroup / 2))
		for i = 0, perGroup - 1 do
			Config.appendUniqueProfileSample(middle, seen, state, middleStart + i)
		end
		local lastStart = math.max(1, total - perGroup + 1)
		for i = 0, perGroup - 1 do
			Config.appendUniqueProfileSample(last, seen, state, lastStart + i)
		end
		local mixedSeed = 1337
		local mixedIterations = 0
		while #mixed < perGroup and mixedIterations < total * 4 do
			mixedSeed = (1103515245 * mixedSeed + 12345) % 2147483647
			local index = (mixedSeed % total) + 1
			local previousCount = #mixed
			Config.appendUniqueProfileSample(mixed, seen, state, index)
			if #mixed == previousCount then
				mixedIterations += 1
			end
		end
		for _, group in ipairs({ first, middle, last, mixed }) do
			for _, sample in ipairs(group) do
				combined[#combined + 1] = sample
			end
		end
		local classCounts = {}
		for _, sample in ipairs(combined) do
			classCounts[sample.className] = (classCounts[sample.className] or 0) + 1
		end
		return {
			totalInstances = total,
			first = first,
			middle = middle,
			last = last,
			mixed = mixed,
			combined = combined,
			classCounts = classCounts,
		}
	end

	function Config.buildProfilePropertyPairs(state, samples)
		local pairsOut = {}
		for _, sample in ipairs(samples) do
			local hotSchema = Config.getHotPropertySchema(state, sample.className)
			for i = 1, hotSchema.count do
				local propertyName = hotSchema.names[i]
				if hotSchema.defaults[i] ~= nil then
					pairsOut[#pairsOut + 1] = {
						instance = sample.instance,
						className = sample.className,
						propertyName = propertyName,
					}
					break
				end
			end
		end
		return pairsOut
	end

	function Config.appendProfileOperation(target, name, metrics)
		target[name] = metrics
	end

	function Config.profileJsonEncodePayload(payload, iterations)
		iterations = math.max(1, math.min(iterations, 5))
		return Config.profileTimedOperation(iterations, function()
			HttpService:JSONEncode(payload)
			return 1
		end)
	end

	function Config.makeSerializerProfileCase(name, typeId, value, enumType)
		return {
			name = name,
			typeId = typeId,
			value = value,
			enumType = enumType or false,
		}
	end

	function Config.buildSerializerProfileCases(state, samples)
		local cases = {
			Config.makeSerializerProfileCase("bool", COMPACT_TYPE_IDS.Bool, true, nil),
			Config.makeSerializerProfileCase("number", COMPACT_TYPE_IDS.Number, 123.456, nil),
			Config.makeSerializerProfileCase("string", COMPACT_TYPE_IDS.String, "RobloxSyncProfile", nil),
			Config.makeSerializerProfileCase("content", COMPACT_TYPE_IDS.ContentId, "rbxassetid://12345", nil),
			Config.makeSerializerProfileCase("vector2", COMPACT_TYPE_IDS.Vector2, Vector2.new(1, 2), nil),
			Config.makeSerializerProfileCase("vector3", COMPACT_TYPE_IDS.Vector3, Vector3.new(1, 2, 3), nil),
			Config.makeSerializerProfileCase("udim", COMPACT_TYPE_IDS.UDim, UDim.new(1, 2), nil),
			Config.makeSerializerProfileCase("udim2", COMPACT_TYPE_IDS.UDim2, UDim2.new(1, 2, 3, 4), nil),
			Config.makeSerializerProfileCase("color3", COMPACT_TYPE_IDS.Color3, Color3.new(0.1, 0.2, 0.3), nil),
			Config.makeSerializerProfileCase("brickcolor", COMPACT_TYPE_IDS.BrickColor, BrickColor.new("Bright red"), nil),
			Config.makeSerializerProfileCase("enum_material", COMPACT_TYPE_IDS.EnumItem, Enum.Material.Plastic, "Enum.Material"),
			Config.makeSerializerProfileCase("cframe", COMPACT_TYPE_IDS.CFrame, CFrame.new(1, 2, 3), nil),
			Config.makeSerializerProfileCase("rect", COMPACT_TYPE_IDS.Rect, Rect.new(0, 1, 2, 3), nil),
			Config.makeSerializerProfileCase(
				"number_sequence",
				COMPACT_TYPE_IDS.NumberSequence,
				NumberSequence.new({
					NumberSequenceKeypoint.new(0, 1, 0),
					NumberSequenceKeypoint.new(1, 2, 0),
				}),
				nil
			),
			Config.makeSerializerProfileCase(
				"color_sequence",
				COMPACT_TYPE_IDS.ColorSequence,
				ColorSequence.new({
					ColorSequenceKeypoint.new(0, Color3.new(1, 0, 0)),
					ColorSequenceKeypoint.new(1, Color3.new(0, 0, 1)),
				}),
				nil
			),
		}
		local okFont, fontValue = pcall(function()
			return Font.new("rbxasset://fonts/families/SourceSansPro.json", Enum.FontWeight.Regular, Enum.FontStyle.Normal)
		end)
		if okFont then
			cases[#cases + 1] = Config.makeSerializerProfileCase("font", COMPACT_TYPE_IDS.Font, fontValue, nil)
		end
		if #samples > 0 then
			cases[#cases + 1] = Config.makeSerializerProfileCase("ref_internal", COMPACT_TYPE_IDS.Ref, samples[1].instance, nil)
		end
		local workspaceService = game:FindFirstChild("Workspace")
		if workspaceService ~= nil and #samples > 0 and workspaceService ~= samples[1].instance then
			cases[#cases + 1] = Config.makeSerializerProfileCase("ref_external", COMPACT_TYPE_IDS.Ref, workspaceService, nil)
		end
		return cases
	end

	function Config.buildJsonShapePayloads(sampleCount)
		local rowCount = math.max(64, math.min(2048, sampleCount * 16))
		local strings = { "Part", "Model", "Position", "Color", "Anchored", "Folder", "Hello\\nWorld" }
		local objectRows = table.create(rowCount)
		local compactRows = table.create(rowCount)
		local sparseRows = table.create(rowCount)
		local shapeRows = table.create(rowCount)
		local verboseTypedRows = table.create(rowCount)
		local internedRows = table.create(rowCount)
		for i = 1, rowCount do
			objectRows[i] = {
				name = "Part",
				className = "Part",
				parentIndex = 0,
				properties = {
					Position = { _type = "Vector3", x = 1, y = 2, z = 3 },
					Color = { _type = "Color3", r = 1, g = 0, b = 0 },
					Anchored = true,
				},
			}
			compactRows[i] = { 1, 1, 0, false, false, { 7 }, { { 1, 2, 3 }, { 1, 0, 0 }, true } }
			sparseRows[i] = { 1, 1, 0, false, false, 1, { 0, { 1, 2, 3 }, 2, true } }
			shapeRows[i] = { 1, 1, 0, false, false, 1, { { 1, 2, 3 }, true } }
			verboseTypedRows[i] = {
				name = "Part",
				className = "Part",
				parentIndex = 0,
				properties = {
					Position = { _type = "Vector3", x = 1, y = 2, z = 3 },
					Color = { _type = "Color3", r = 1, g = 0, b = 0 },
				},
				source = "print(\"Hello\\nWorld\")",
			}
			internedRows[i] = { 1, 1, 0, false, false, { 3 }, { 1, 2 } }
		end
		return {
			object_rows = { format = "object-v1", items = objectRows },
			compact_rows = { format = "compact-v5", strings = strings, items = compactRows },
			sparse_rows = { format = "compact-v6", strings = strings, items = sparseRows },
			shape_rows = { format = "compact-v6-shape", strings = strings, shapes = { { 1, { 0, 2 } } }, items = shapeRows },
			verbose_typed_rows = { format = "verbose-typed", items = verboseTypedRows },
			interned_rows = { format = "interned", strings = strings, items = internedRows },
			source_text_rows = {
				format = "source-v1",
				items = {
					1,
					"print(\"hello\\nworld\")\nlocal path = [[C:\\\\tmp\\\\file]]",
					2,
					"return \"quotes \\\" and backslashes \\\\\"",
				},
			},
			rowCount = rowCount,
		}
	end

	function Config.profilePluginOps(serviceName, sampleCount, iterations, rawFlags)
		local state = Config.ensureProfileState(serviceName)
		local flags = Config.parseProfileFlags(rawFlags)
		local profileAll = next(flags) == nil
		local includesHeavyShape = profileAll or Config.profileFlagEnabled(flags, "json") or Config.profileFlagEnabled(flags, "buffer")
		local maxSamples = if includesHeavyShape then 256 else 1024
		local maxIterations = if includesHeavyShape then 11 else 256
		local profileSampleCount = math.max(8, math.min(maxSamples, sampleCount or 64))
		local profileIterations = math.max(3, math.min(maxIterations, iterations or 7))
		local samples = Config.buildProfileSamples(state, profileSampleCount)
		local combined = samples.combined
		local operations = {}
		local projectedCallCount = 0
		for i = 1, #state.instances do
			local className = state.classNameByIndex[i]
			if className ~= nil then
				projectedCallCount += Config.getHotPropertySchema(state, className).count
			end
		end
		projectedCallCount = math.max(1, projectedCallCount)
		local summary = {
			service = serviceName,
			bridgeVersion = context.bridgeVersion,
			bridgeBuildUnix = context.bridgeBuildUnix,
			protocolVersion = context.protocolVersion,
			codecVersion = context.codecVersion,
			instanceCount = #state.instances,
			sampleCount = #combined,
			iterations = profileIterations,
			projectedPropertyReads = projectedCallCount,
			projectedServerStoragePropertyReads = projectedCallCount,
			classCounts = samples.classCounts,
			groupSizes = {
				first = #samples.first,
				middle = #samples.middle,
				last = #samples.last,
				mixed = #samples.mixed,
			},
		}

		if Config.profileFlagEnabled(flags, "luau") then
			local numericLoopCount = math.max(64, #combined * 8)
			Config.appendProfileOperation(operations, "luau.empty_numeric_loop", Config.profileFixedCountOperation(numericLoopCount, profileIterations, function() end))
			local emptyFn = function() end
			Config.appendProfileOperation(operations, "luau.empty_function_call", Config.profileFixedCountOperation(math.max(64, #combined * 8), profileIterations, function()
				emptyFn()
			end))
			local fn = function(sample)
				return sample.index
			end
			local closureOffset = 1
			local closure = function(sample)
				return sample.index + closureOffset
			end
			Config.appendProfileOperation(operations, "luau.function_call", Config.profileSampleOperation(combined, profileIterations, function(sample)
				fn(sample)
			end))
			Config.appendProfileOperation(operations, "luau.closure_call", Config.profileSampleOperation(combined, profileIterations, function(sample)
				closure(sample)
			end))
			Config.appendProfileOperation(operations, "luau.empty_pcall", Config.profileFixedCountOperation(math.max(64, #combined), profileIterations, function()
				pcall(emptyFn)
			end))
			Config.appendProfileOperation(operations, "luau.pcall_noop", Config.profileSampleOperation(combined, profileIterations, function()
				pcall(function() end)
			end))
			for _, size in ipairs({ 2, 4, 8, 16, 32 }) do
				Config.appendProfileOperation(operations, "luau.table_create_" .. tostring(size), Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
					table.create(size)
				end))
			end
			Config.appendProfileOperation(operations, "luau.table_append", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function(i)
				local out = {}
				out[#out + 1] = i
			end))
			Config.appendProfileOperation(operations, "luau.local_counter_increment", Config.profileFixedCountOperation(math.max(64, #combined * 8), profileIterations, function()
				local counter = 0
				counter += 1
			end))
			Config.appendProfileOperation(operations, "luau.bump_export_metric", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
				local metricsState = {
					exportMetrics = Config.newExportMetrics(),
					exportMetricsSinceLastRead = Config.newExportMetrics(),
				}
				Config.bumpExportMetric(metricsState, "propertiesRead")
			end))
			Config.appendProfileOperation(operations, "luau.bit32_mask_set", Config.profileFixedCountOperation(math.max(64, #combined * 4), profileIterations, function(i)
				local mask = 0
				mask = bit32.bor(mask, bit32.lshift(1, i % 31))
			end))
			Config.appendProfileOperation(operations, "luau.typeof_vector3", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
				typeof(Vector3.new(1, 2, 3))
			end))
			Config.appendProfileOperation(operations, "luau.type_string", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
				type("RobloxSyncProfile")
			end))
			Config.appendProfileOperation(operations, "luau.tostring_name", Config.profileSampleOperation(combined, profileIterations, function(sample)
				tostring(sample.name)
			end))
			Config.appendProfileOperation(operations, "luau.string_lower_name", Config.profileSampleOperation(combined, profileIterations, function(sample)
				string.lower(sample.name)
			end))
			local stringDict = {}
			local numericDict = {}
			local arrayValues = table.create(#combined)
			for i, sample in ipairs(combined) do
				stringDict[sample.name .. ":" .. tostring(i)] = i
				numericDict[i] = sample.index
				arrayValues[i] = sample.index
			end
			Config.appendProfileOperation(operations, "luau.dict_lookup_string", Config.profileSampleOperation(combined, profileIterations, function(sample, i)
				local _ = stringDict[sample.name .. ":" .. tostring(i)]
			end))
			Config.appendProfileOperation(operations, "luau.dict_lookup_numeric", Config.profileFixedCountOperation(#combined, profileIterations, function(i)
				local _ = numericDict[i]
			end))
			Config.appendProfileOperation(operations, "luau.array_lookup_numeric", Config.profileFixedCountOperation(#combined, profileIterations, function(i)
				local _ = arrayValues[i]
			end))
			local fallbackLookupSamples = {}
			for i = 1, math.min(#combined, 128) do
				local sample = combined[i]
				fallbackLookupSamples[i] = {
					propertyName = "Name",
					fallbackMap = Config.getClassPropertyFallbackMap(state, sample.className),
				}
			end
			if #fallbackLookupSamples > 0 then
				Config.appendProfileOperation(operations, "luau.fallback_map_lookup", Config.profileSampleOperation(fallbackLookupSamples, profileIterations, function(sample)
					local _ = sample.fallbackMap[sample.propertyName]
				end))
			end
			Config.appendProfileOperation(operations, "luau.string_intern_miss", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function(i)
				local strings = {}
				local stringIds = {}
				Config.internBatchString(strings, stringIds, "ProfileMiss" .. tostring(i))
			end))
			Config.appendProfileOperation(operations, "luau.string_intern_hit", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
				local strings = { "ProfileHit" }
				local stringIds = { ProfileHit = 1 }
				Config.internBatchString(strings, stringIds, "ProfileHit")
			end))
		end

		if Config.profileFlagEnabled(flags, "instance") or Config.profileFlagEnabled(flags, "read") then
			Config.appendProfileOperation(operations, "instance.property_read_name_generic", Config.profileSampleOperation(combined, profileIterations, function(sample)
				local _ = sample.instance["Name"]
			end))
			Config.appendProfileOperation(operations, "instance.property_read_name_pcall", Config.profileSampleOperation(combined, profileIterations, function(sample)
				pcall(function()
					return sample.instance["Name"]
				end)
			end))
			Config.appendProfileOperation(operations, "instance.field_name", Config.profileSampleOperation(combined, profileIterations, function(sample)
				local _ = sample.instance.Name
			end))
			Config.appendProfileOperation(operations, "instance.field_class_name", Config.profileSampleOperation(combined, profileIterations, function(sample)
				local _ = sample.instance.ClassName
			end))
			Config.appendProfileOperation(operations, "instance.field_parent", Config.profileSampleOperation(combined, profileIterations, function(sample)
				local _ = sample.instance.Parent
			end))
			Config.appendProfileOperation(operations, "instance.cached_instance_index", Config.profileSampleOperation(combined, profileIterations, function(sample)
				local _ = IdentityModule.getCachedInstanceIndex(state, sample.instance)
			end))
			Config.appendProfileOperation(operations, "instance.cached_parent_index", Config.profileSampleOperation(combined, profileIterations, function(sample)
				local _ = IdentityModule.getCachedParentInstanceIndex(state, sample.instance)
			end))
			local positionSamples = {}
			for _, sample in ipairs(combined) do
				local ok = pcall(function()
					local _ = sample.instance.Position
				end)
				if ok then
					positionSamples[#positionSamples + 1] = sample
				end
			end
			if #positionSamples > 0 then
				Config.appendProfileOperation(operations, "instance.property_read_position_dot", Config.profileSampleOperation(positionSamples, profileIterations, function(sample)
					local _ = sample.instance.Position
				end))
				Config.appendProfileOperation(operations, "instance.property_read_position_bracket", Config.profileSampleOperation(positionSamples, profileIterations, function(sample)
					local _ = sample.instance["Position"]
				end))
			end
			Config.appendProfileOperation(operations, "instance.isa_lua_source", Config.profileSampleOperation(combined, profileIterations, function(sample)
				local _ = sample.instance:IsA("LuaSourceContainer")
			end))
			Config.appendProfileOperation(operations, "instance.lua_source_class_lookup", Config.profileSampleOperation(combined, profileIterations, function(sample)
				local _ = Config.LUA_SOURCE_CLASS[sample.className] == true
			end))
			local withAttributes = {}
			local withoutAttributes = {}
			for _, sample in ipairs(combined) do
				local attrs = sample.instance:GetAttributes()
				if next(attrs) == nil then
					withoutAttributes[#withoutAttributes + 1] = sample
				else
					withAttributes[#withAttributes + 1] = sample
				end
			end
			if #withoutAttributes > 0 then
				Config.appendProfileOperation(operations, "instance.get_attributes_empty", Config.profileSampleOperation(withoutAttributes, profileIterations, function(sample)
					sample.instance:GetAttributes()
				end))
				Config.appendProfileOperation(operations, "instance.get_attributes_empty_next", Config.profileSampleOperation(withoutAttributes, profileIterations, function(sample)
					local attrs = sample.instance:GetAttributes()
					next(attrs)
				end))
			end
			if #withAttributes > 0 then
				Config.appendProfileOperation(operations, "instance.get_attributes_nonempty", Config.profileSampleOperation(withAttributes, profileIterations, function(sample)
					sample.instance:GetAttributes()
				end))
			end
		end

		if Config.profileFlagEnabled(flags, "modified") then
			local propertyPairs = Config.buildProfilePropertyPairs(state, combined)
			local directEligible = {}
			local directFailures = 0
			local falseCount = 0
			local trueCount = 0
			local failureCount = 0
			for _, pair in ipairs(propertyPairs) do
				local ok, modified = pcall(function()
					return pair.instance:IsPropertyModified(pair.propertyName)
				end)
				if ok and type(modified) == "boolean" then
					directEligible[#directEligible + 1] = pair
					if modified then
						trueCount += 1
					else
						falseCount += 1
					end
				else
					directFailures += 1
					failureCount += 1
				end
			end
			summary.modifiedDefaultProbe = {
				pairCount = #propertyPairs,
				directEligibleCount = #directEligible,
				directFailureCount = directFailures,
				falseCount = falseCount,
				trueCount = trueCount,
				failureCount = failureCount,
				defaultFalseRate = #directEligible > 0 and falseCount / #directEligible or 0,
			}
			if #directEligible > 0 then
				Config.appendProfileOperation(operations, "modified.direct_valid", Config.profileSampleOperation(directEligible, profileIterations, function(pair)
					pair.instance:IsPropertyModified(pair.propertyName)
				end))
				Config.appendProfileOperation(operations, "modified.pcall_valid", Config.profileSampleOperation(directEligible, profileIterations, function(pair)
					pcall(function()
						return pair.instance:IsPropertyModified(pair.propertyName)
					end)
				end))
				Config.appendProfileOperation(operations, "modified.invalid_pcall", Config.profileSampleOperation(directEligible, profileIterations, function(pair)
					pcall(function()
						return pair.instance:IsPropertyModified("__ROBLOX_SYNC_INVALID_PROPERTY__")
					end)
				end))
				Config.appendProfileOperation(operations, "modified.validation_read", Config.profileSampleOperation(directEligible, profileIterations, function(pair)
					local ok, modified = pcall(function()
						return pair.instance:IsPropertyModified(pair.propertyName)
					end)
					if ok and modified == false then
						context.tryRead(pair.instance, pair.propertyName)
					end
				end))
			end
		end

		if Config.profileFlagEnabled(flags, "engine") then
			local serviceInstance = game:FindFirstChild(serviceName)
			if serviceInstance ~= nil then
				local okSerializationService, serializationService = pcall(function()
					return game:GetService("SerializationService")
				end)
				if okSerializationService and serializationService ~= nil then
					local serializedLen = 0
					local serializeError = nil
					Config.appendProfileOperation(operations, "engine.serialization_service", Config.profileTimedOperation(1, function()
						local okSerialize, payload = pcall(function()
							return serializationService:SerializeInstancesAsync({ serviceInstance })
						end)
						if okSerialize and type(buffer) == "table" and typeof(payload) == "buffer" then
							serializedLen = buffer.len(payload)
						elseif not okSerialize then
							serializeError = tostring(payload)
						end
						return 1
					end))
					summary.engineSerializationServiceBytes = serializedLen
					if serializeError ~= nil then
						summary.engineSerializationServiceError = serializeError
					end
					local childrenPayloadLen = 0
					local childrenSerializeError = nil
					Config.appendProfileOperation(operations, "engine.serialization_service_children", Config.profileTimedOperation(1, function()
						local children = serviceInstance:GetChildren()
						local okSerialize, payload = pcall(function()
							return serializationService:SerializeInstancesAsync(children)
						end)
						if okSerialize and type(buffer) == "table" and typeof(payload) == "buffer" then
							childrenPayloadLen = buffer.len(payload)
						elseif not okSerialize then
							childrenSerializeError = tostring(payload)
						end
						return 1
					end))
					summary.engineSerializationServiceChildrenBytes = childrenPayloadLen
					if childrenSerializeError ~= nil then
						summary.engineSerializationServiceChildrenError = childrenSerializeError
					end
				else
					operations["engine.serialization_service"] = {
						skipped = true,
						error = tostring(serializationService),
					}
				end

				local okStudioService, studioService = pcall(function()
					return game:GetService("StudioService")
				end)
				if okStudioService and studioService ~= nil then
					local serializedLen = 0
					local serializeError = nil
					Config.appendProfileOperation(operations, "engine.studio_service_serialize", Config.profileTimedOperation(1, function()
						local okSerialize, payload = pcall(function()
							return studioService:SerializeInstances({ serviceInstance })
						end)
						if okSerialize and type(payload) == "string" then
							serializedLen = #payload
						elseif not okSerialize then
							serializeError = tostring(payload)
						end
						return 1
					end))
					summary.engineStudioServiceBytes = serializedLen
					if serializeError ~= nil then
						summary.engineStudioServiceError = serializeError
					end
				else
					operations["engine.studio_service_serialize"] = {
						skipped = true,
						error = tostring(studioService),
					}
				end
			else
				operations["engine.serialization_service"] = {
					skipped = true,
					error = "service not found",
				}
				operations["engine.studio_service_serialize"] = {
					skipped = true,
					error = "service not found",
				}
			end
		end

		if Config.profileFlagEnabled(flags, "serialize") then
			local serializerCases = Config.buildSerializerProfileCases(state, combined)
			for _, caseInfo in ipairs(serializerCases) do
				Config.appendProfileOperation(operations, "serialize.generic." .. caseInfo.name, Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
					context.serializeValue(caseInfo.value, state)
				end))
				Config.appendProfileOperation(operations, "serialize.schema." .. caseInfo.name, Config.profileTimedOperation(profileIterations, function()
					local calls = math.max(16, #combined)
					local strings = {}
					local stringIds = {}
					for _ = 1, calls do
						context.encodeSchemaValueV5(caseInfo.typeId, caseInfo.enumType ~= false and caseInfo.enumType or nil, caseInfo.value, state, strings, stringIds)
					end
					return calls
				end))
			end
			Config.appendProfileOperation(operations, "serialize.attributes.empty", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
				context.serializeAttributesCompactV5({}, state, {}, {})
			end))
			Config.appendProfileOperation(operations, "serialize.attributes.nonempty", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
				context.serializeAttributesCompactV5({
					Health = 100,
					DisplayName = "Profile",
					Origin = Vector3.new(1, 2, 3),
				}, state, {}, {})
			end))
			local vector3Value = Vector3.new(1, 2, 3)
			Config.appendProfileOperation(operations, "serialize.compare.vector3_with_typeof", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
				if typeof(vector3Value) == "Vector3" then
					local _ = vector3Value.X == 1 and vector3Value.Y == 2 and vector3Value.Z == 3
				end
			end))
			Config.appendProfileOperation(operations, "serialize.compare.vector3_direct", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
				local _ = vector3Value.X == 1 and vector3Value.Y == 2 and vector3Value.Z == 3
			end))
			local enumValue = Enum.Material.Plastic
			Config.appendProfileOperation(operations, "serialize.compare.enum_name", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
				local _ = enumValue.Name == "Plastic"
			end))
			Config.appendProfileOperation(operations, "serialize.compare.enum_value", Config.profileFixedCountOperation(math.max(16, #combined), profileIterations, function()
				local _ = enumValue.Value == Enum.Material.Plastic.Value
			end))
			local exportSamples = {}
			for i = 1, math.min(#combined, 64) do
				exportSamples[i] = combined[i]
			end
			if #exportSamples > 0 then
				Config.appendProfileOperation(operations, "serialize.exporter_direct", Config.profileSampleOperation(exportSamples, profileIterations, function(sample)
					context.exportCompactV5InstanceInternal(state, sample.instance, sample.index, false, {}, {})
				end))
				Config.appendProfileOperation(operations, "serialize.exporter_pcall", Config.profileSampleOperation(exportSamples, profileIterations, function(sample)
					pcall(context.exportCompactV5InstanceInternal, state, sample.instance, sample.index, false, {}, {})
				end))
			end
		end

		if Config.profileFlagEnabled(flags, "json") then
			local payloads = Config.buildJsonShapePayloads(#combined)
			local rowCount = payloads.rowCount or 0
			summary.jsonShapeRowCount = rowCount
			payloads.rowCount = nil
			for name, payload in pairs(payloads) do
				Config.appendProfileOperation(operations, "json." .. tostring(name), Config.profileJsonEncodePayload(payload, profileIterations))
			end
			local compactNoSourceRows = table.create(rowCount)
			local compactNoSourceNoAttrsRows = table.create(rowCount)
			local compactSideTableRows = table.create(rowCount)
			local sideTableAttributes = {}
			local sideTableSources = {}
			for i = 1, rowCount do
				compactNoSourceRows[i] = { 1, 1, 0, false, 3, { 1, 2 } }
				compactNoSourceNoAttrsRows[i] = { 1, 1, 0, 3, { 1, 2 } }
				compactSideTableRows[i] = { 1, 1, 0, 3, { 1, 2 } }
				if i % 64 == 0 then
					sideTableAttributes[#sideTableAttributes + 1] = i
					sideTableAttributes[#sideTableAttributes + 1] = { 1, COMPACT_TYPE_IDS.Number, 42 }
					sideTableSources[#sideTableSources + 1] = i
					sideTableSources[#sideTableSources + 1] = i
				end
			end
			Config.appendProfileOperation(operations, "json.compact_rows_no_source", Config.profileJsonEncodePayload({
				format = "compact-v6-no-source",
				strings = { "Part" },
				items = compactNoSourceRows,
			}, profileIterations))
			Config.appendProfileOperation(operations, "json.compact_rows_no_source_no_attributes", Config.profileJsonEncodePayload({
				format = "compact-v6-no-source-no-attrs",
				strings = { "Part" },
				items = compactNoSourceNoAttrsRows,
			}, profileIterations))
			Config.appendProfileOperation(operations, "json.compact_rows_side_tables", Config.profileJsonEncodePayload({
				format = "compact-v6-side-tables",
				strings = { "Part" },
				items = compactSideTableRows,
				attributes = sideTableAttributes,
				sources = sideTableSources,
			}, profileIterations))
		end

		if Config.profileFlagEnabled(flags, "buffer") and type(buffer) == "table" and type(buffer.create) == "function" then
			Config.appendProfileOperation(operations, "buffer.create_512", Config.profileFixedCountOperation(64, profileIterations, function()
				buffer.create(512)
			end))
			Config.appendProfileOperation(operations, "buffer.write_u8_256", Config.profileTimedOperation(profileIterations, function()
				local calls = 256
				local buf = buffer.create(calls)
				for i = 0, calls - 1 do
					buffer.writeu8(buf, i, i % 256)
				end
				return calls
			end))
			Config.appendProfileOperation(operations, "buffer.write_f64_64", Config.profileTimedOperation(profileIterations, function()
				local calls = 64
				local buf = buffer.create(calls * 8)
				for i = 0, calls - 1 do
					buffer.writef64(buf, i * 8, i + 0.5)
				end
				return calls
			end))
			Config.appendProfileOperation(operations, "buffer.write_string_128", Config.profileTimedOperation(profileIterations, function()
				local text = "RobloxSyncBufferProfile"
				local calls = 32
				local buf = buffer.create(#text * calls)
				for i = 0, calls - 1 do
					buffer.writestring(buf, i * #text, text)
				end
				return calls
			end))
			Config.appendProfileOperation(operations, "buffer.to_string_512", Config.profileTimedOperation(profileIterations, function()
				local buf = buffer.create(512)
				buffer.writeu8(buf, 0, 1)
				buffer.tostring(buf)
				return 1
			end))
			for _, size in ipairs({ 64 * 1024, 512 * 1024, 2 * 1024 * 1024, 8 * 1024 * 1024 }) do
				Config.appendProfileOperation(operations, "buffer.to_string_" .. tostring(size), Config.profileTimedOperation(profileIterations, function()
					local buf = buffer.create(size)
					buffer.writeu8(buf, 0, 1)
					buffer.tostring(buf)
					return 1
				end))
			end
			local okJsonEncode, jsonEncodeError = pcall(function()
				local buf = buffer.create(32)
				HttpService:JSONEncode(buf)
			end)
			if okJsonEncode then
				Config.appendProfileOperation(operations, "buffer.json_encode_32", Config.profileTimedOperation(profileIterations, function()
					local buf = buffer.create(32)
					HttpService:JSONEncode(buf)
					return 1
				end))
			else
				operations["buffer.json_encode_32"] = { skipped = true, error = tostring(jsonEncodeError) }
			end
			operations["buffer.raw_websocket_probe"] = {
				skipped = true,
				reason = "requires bridge roundtrip validation outside profilePluginOps",
			}
		end

		return {
			service = serviceName,
			profile = summary,
			flags = flags,
			operations = operations,
		}
	end
end

return BridgeProfiling
