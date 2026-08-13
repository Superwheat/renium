local BridgeCreatorApi = {}

local function splitPath(text: string): { string }
	local segments = {}
	local start = 1
	local index = 1
	while index <= #text do
		local character = string.sub(text, index, index)
		if character == "\\" then
			index += 2
		elseif character == "." then
			segments[#segments + 1] = string.sub(text, start, index - 1)
			start = index + 1
			index += 1
		else
			index += 1
		end
	end
	segments[#segments + 1] = string.sub(text, start)
	return segments
end

local function parseSegment(text: string): (string, number?)
	local name, ordinal = string.match(text, "^(.*)%[(%d+)%]$")
	if name == nil then
		name = text
	end
	name = string.gsub(name, "\\(.)", "%1")
	return name, if ordinal then tonumber(ordinal) else nil
end

local function resolvePath(path: any): Instance
	local text = tostring(path or "Workspace")
	local segments = splitPath(text)
	local current: Instance = game
	local index = 1
	local first = parseSegment(segments[1] or "")
	if first == "game" then
		index = 2
	elseif first == "Workspace" or first == "workspace" then
		current = game:GetService("Workspace")
		index = 2
	end
	while index <= #segments do
		local name, ordinal = parseSegment(segments[index])
		local matches = {}
		for _, child in ipairs(current:GetChildren()) do
			if child.Name == name then
				matches[#matches + 1] = child
			end
		end
		if ordinal == nil and #matches > 1 then
			error(
				("Path segment '%s' matched %d children under %s; add [n]"):format(
					name,
					#matches,
					current:GetFullName()
				)
			)
		end
		current = matches[ordinal or 1]
		if current == nil then
			error(("Path segment '%s' wasn't found under %s"):format(segments[index], text))
		end
		index += 1
	end
	return current
end

local function instanceResult(instance: Instance): { [string]: any }
	return {
		name = instance.Name,
		className = instance.ClassName,
		path = instance:GetFullName(),
	}
end

local function finishRecording(history: ChangeHistoryService, recording: string?, commit: boolean)
	if recording then
		history:FinishRecording(
			recording,
			if commit then Enum.FinishRecordingOperation.Commit else Enum.FinishRecordingOperation.Cancel
		)
	end
end

local function replaceText(source: string, old: string, new: string, replaceAll: boolean): string
	local start = string.find(source, old, 1, true)
	if start == nil then
		error("old_string was not found")
	end
	if not replaceAll then
		return string.sub(source, 1, start - 1) .. new .. string.sub(source, start + #old)
	end
	local output = {}
	local cursor = 1
	while start do
		output[#output + 1] = string.sub(source, cursor, start - 1)
		output[#output + 1] = new
		cursor = start + #old
		start = string.find(source, old, cursor, true)
	end
	output[#output + 1] = string.sub(source, cursor)
	return table.concat(output)
end

function BridgeCreatorApi.create()
	local api = {}
	local history = game:GetService("ChangeHistoryService")
	local HttpService = game:GetService("HttpService")
	local jobs = {}
	local jobOrder = {}
	local cameraStates = {}

	local function startJob(callback): string?
		if #jobOrder >= 32 then
			for index, existingId in ipairs(jobOrder) do
				if jobs[existingId].status ~= "running" then
					jobs[existingId] = nil
					table.remove(jobOrder, index)
					break
				end
			end
			if #jobOrder >= 32 then
				return nil
			end
		end
		local id = HttpService:GenerateGUID(false)
		jobs[id] = { status = "running", startedAt = os.clock() }
		jobOrder[#jobOrder + 1] = id
		jobs[id].thread = task.spawn(function()
			local ok, result = pcall(callback)
			local job = jobs[id]
			if job == nil then
				return
			end
			job.finishedAt = os.clock()
			if ok then
				job.status = "succeeded"
				job.result = result
			else
				job.status = "failed"
				job.error = tostring(result)
			end
		end)
		return id
	end

	function api.studioState()
		local RunService = game:GetService("RunService")
		return {
			ok = true,
			running = RunService:IsRunning(),
			isEdit = RunService:IsEdit(),
			isClient = RunService:IsClient(),
			isServer = RunService:IsServer(),
			runState = tostring(RunService.RunState),
		}
	end

	function api.creatorContext()
		return {
			ok = true,
			userId = game:GetService("StudioService"):GetUserId(),
			creatorId = game.CreatorId,
			creatorType = tostring(game.CreatorType),
			gameId = game.GameId,
			placeId = game.PlaceId,
		}
	end

	function api.cameraCapture(params)
		local action = tostring(params.action or "")
		if action == "restore" then
			local token = tostring(params.token or "")
			local state = cameraStates[token]
			if state == nil then
				return { ok = false, error = "Camera capture token is stale" }
			end
			cameraStates[token] = nil
			state.camera.CameraType = state.cameraType
			state.camera.CFrame = state.cframe
			state.camera.Focus = state.focus
			return { ok = true }
		end
		if action ~= "prepare" then
			return { ok = false, error = "cameraCapture action must be prepare or restore" }
		end
		local position = params.position
		local lookAt = params.lookAt
		if type(position) ~= "table" or #position ~= 3 or type(lookAt) ~= "table" or #lookAt ~= 3 then
			return { ok = false, error = "Camera position and lookAt must have three numbers" }
		end
		local camera = game:GetService("Workspace").CurrentCamera
		if camera == nil then
			return { ok = false, error = "No CurrentCamera is available" }
		end
		local token = HttpService:GenerateGUID(false)
		cameraStates[token] = {
			camera = camera,
			cameraType = camera.CameraType,
			cframe = camera.CFrame,
			focus = camera.Focus,
		}
		local target = Vector3.new(lookAt[1], lookAt[2], lookAt[3])
		camera.CameraType = Enum.CameraType.Scriptable
		camera.CFrame = CFrame.lookAt(Vector3.new(position[1], position[2], position[3]), target)
		camera.Focus = CFrame.new(target)
		return { ok = true, token = token }
	end

	function api.insertAsset(params)
		local assetId = tonumber(params.assetId)
		if assetId == nil or assetId % 1 ~= 0 or assetId <= 0 then
			return { ok = false, error = "assetId must be a positive integer" }
		end
		local parent = resolvePath(params.parentPath)
		local assetType = tostring(params.assetType or "")
		local name = tostring(params.assetName or params.name or ("Asset" .. assetId))
		local recording = history:TryBeginRecording("ReniumInsertAsset", "Insert asset")
		local ok, result = pcall(function()
			if assetType == "" then
				local info = game:GetService("MarketplaceService"):GetProductInfoAsync(assetId, Enum.InfoType.Asset)
				local typeId = tonumber(info.AssetTypeId)
				for _, item in ipairs(Enum.AssetType:GetEnumItems()) do
					if item.Value == typeId then
						assetType = item.Name
						break
					end
				end
			end
			local instance
			if assetType == "Image" or assetType == "Decal" then
				instance = Instance.new("Decal")
				instance.Texture = "rbxassetid://" .. assetId
			elseif assetType == "Audio" then
				instance = Instance.new("Sound")
				instance.SoundId = "rbxassetid://" .. assetId
			elseif assetType == "Video" then
				instance = Instance.new("VideoFrame")
				instance.Video = "rbxassetid://" .. assetId
			elseif assetType == "Animation" then
				instance = Instance.new("Animation")
				instance.AnimationId = "rbxassetid://" .. assetId
			else
				instance = game:GetService("AssetService"):LoadAssetAsync(assetId)
			end
			instance.Name = name
			instance.Parent = parent
			return instanceResult(instance)
		end)
		finishRecording(history, recording, ok)
		if not ok then
			return { ok = false, error = tostring(result) }
		end
		result.ok = true
		result.assetId = assetId
		return result
	end

	function api.multiEdit(params)
		local path = tostring(params.filePath or params.path or "")
		local edits = params.edits
		if path == "" or type(edits) ~= "table" or #edits == 0 then
			return { ok = false, error = "multiEdit requires filePath and edits" }
		end
		local recording = history:TryBeginRecording("ReniumMultiEdit", "Edit script")
		local found, script = pcall(resolvePath, path)
		local created = false
		if not found then
			local className = tostring(params.className or "")
			if className ~= "Script" and className ~= "LocalScript" and className ~= "ModuleScript" then
				finishRecording(history, recording, false)
				return { ok = false, error = "className must be Script, LocalScript, or ModuleScript for a new script" }
			end
			local segments = splitPath(path)
			local leaf = table.remove(segments)
			local name, ordinal = parseSegment(leaf or "")
			if name == "" or ordinal ~= nil or #segments == 0 then
				finishRecording(history, recording, false)
				return { ok = false, error = "New script path must end in one unambiguous name" }
			end
			local parentFound, parent = pcall(resolvePath, table.concat(segments, "."))
			if not parentFound then
				finishRecording(history, recording, false)
				return { ok = false, error = tostring(parent) }
			end
			script = Instance.new(className)
			script.Name = name
			script.Parent = parent
			created = true
		end
		if not script:IsA("LuaSourceContainer") then
			finishRecording(history, recording, false)
			return { ok = false, error = path .. " is not a script" }
		end
		local ok, result = pcall(function()
			game:GetService("ScriptEditorService"):UpdateSourceAsync(script, function(source)
				local output = source
				for index, edit in ipairs(edits) do
					if type(edit) ~= "table" then
						error(("edits[%d] must be an object"):format(index))
					end
					local old = edit.oldString or edit.old_string
					local new = edit.newString or edit.new_string
					if type(old) ~= "string" or type(new) ~= "string" or old == new then
						error(("edits[%d] needs different oldString and newString values"):format(index))
					end
					if old == "" then
						if not created or index ~= 1 or output ~= "" then
							error("An empty oldString is only valid for the first edit of a new empty script")
						end
						output = new
					else
						output = replaceText(output, old, new, edit.replaceAll == true or edit.replace_all == true)
					end
				end
				return output
			end)
			return instanceResult(script)
		end)
		if not ok then
			if created then
				script:Destroy()
			end
			finishRecording(history, recording, false)
			return { ok = false, error = tostring(result) }
		end
		finishRecording(history, recording, true)
		result.ok = true
		result.edits = #edits
		return result
	end

	function api.generateModel(params)
		local prompt = tostring(params.prompt or params.textPrompt or "")
		local imageReference = params.imageAssetId or params.imageId or params.imageUri
		if prompt == "" and imageReference == nil then
			return { ok = false, error = "Generation requires prompt or imageAssetId" }
		end
		local parentPath = params.parentPath
		local name = tostring(params.name or "GeneratedModel")
		local jobId = startJob(function()
			local inputs = {}
			if prompt ~= "" then
				inputs.TextPrompt = prompt
			end
			if imageReference ~= nil then
				local imageAssetId = tonumber(imageReference)
					or tonumber(string.match(tostring(imageReference), "(%d+)$"))
				if imageAssetId == nil or imageAssetId % 1 ~= 0 or imageAssetId <= 0 then
					error("imageAssetId must be a positive integer")
				end
				inputs.Image = Content.fromAssetId(imageAssetId)
			end
			if params.size ~= nil then
				if type(params.size) ~= "table" then
					error("size must have three numbers")
				end
				local x = params.size[1] or params.size.x
				local y = params.size[2] or params.size.y
				local z = params.size[3] or params.size.z
				if type(x) ~= "number" or type(y) ~= "number" or type(z) ~= "number" then
					error("size must have three numbers")
				end
				inputs.Size = Vector3.new(x, y, z)
			end
			if params.maxTriangles ~= nil then
				local maxTriangles = tonumber(params.maxTriangles)
				if maxTriangles == nil or maxTriangles % 1 ~= 0 or maxTriangles < 12 or maxTriangles > 20000 then
					error("maxTriangles must be an integer from 12 through 20000")
				end
				inputs.MaxTriangles = maxTriangles
			end
			if params.generateTextures ~= nil then
				inputs.GenerateTextures = not not params.generateTextures
			end
			local parts = params.parts or params.partNames
			if type(parts) == "string" then
				local names = {}
				for part in string.gmatch(parts, "[^,]+") do
					local trimmed = string.match(part, "^%s*(.-)%s*$")
					if trimmed ~= "" then
						names[#names + 1] = trimmed
					end
				end
				parts = names
			end
			if params.segmentation == "explicit" and (type(parts) ~= "table" or #parts == 0) then
				error("Explicit segmentation requires parts")
			end
			local schema
			if type(parts) == "table" and #parts > 0 then
				if #parts > 8 then
					error("Generation supports at most eight named parts")
				end
				schema = { SchemaDefinition = { Groups = parts } }
			else
				schema = { PredefinedSchema = "Body1" }
			end
			local recording = history:TryBeginRecording("ReniumGenerateModel", "Generate model")
			local ok, generated, metadata = pcall(function()
				local model, generationMetadata = game:GetService("GenerationService")
					:GenerateModelAsync(inputs, schema)
				if params.anchored ~= false then
					for _, descendant in ipairs(model:GetDescendants()) do
						if descendant:IsA("BasePart") then
							descendant.Anchored = true
						end
					end
				end
				model.Name = name
				model.Parent = resolvePath(parentPath)
				return model, generationMetadata
			end)
			finishRecording(history, recording, ok)
			if not ok then
				error(generated)
			end
			local result = instanceResult(generated)
			if type(metadata) == "table" then
				result.generationId = metadata.UUID or metadata.Uuid or metadata.uuid or metadata.GenerationId
			end
			return result
		end)
		if jobId == nil then
			return { ok = false, error = "Too many generation jobs are still running" }
		end
		return { ok = true, jobId = jobId, status = "running" }
	end

	function api.uploadImages(params)
		local images = params.images or params.imagePaths
		if type(images) ~= "table" or #images == 0 or #images > 20 then
			return { ok = false, error = "uploadImages requires 1 through 20 image URIs" }
		end
		for index, source in ipairs(images) do
			if type(source) ~= "string" or not string.match(source, "^https?://") then
				return { ok = false, error = ("images[%d] must be an HTTP or HTTPS URI"):format(index) }
			end
		end
		local name = tostring(params.name or "Renium image")
		local description = tostring(params.description or "")
		local jobId = startJob(function()
			local AssetService = game:GetService("AssetService")
			local output = {}
			for index, source in ipairs(images) do
				local image = AssetService:CreateEditableImageAsync(Content.fromUri(source))
				if image == nil then
					error(("Roblox could not load images[%d]"):format(index))
				end
				local result, id = AssetService:CreateAssetAsync(image, Enum.AssetType.Image, {
					Name = if #images == 1 then name else name .. " " .. index,
					Description = description,
				})
				image:Destroy()
				if result ~= Enum.CreateAssetResult.Success then
					error(("Image upload %d failed: %s (%s)"):format(index, tostring(result), tostring(id)))
				end
				output[#output + 1] = {
					source = source,
					assetId = id,
					uri = "rbxassetid://" .. id,
				}
			end
			return { images = output }
		end)
		if jobId == nil then
			return { ok = false, error = "Too many creator jobs are still running" }
		end
		return { ok = true, jobId = jobId, status = "running" }
	end

	function api.creatorJob(params)
		local id = tostring(params.jobId or "")
		local job = jobs[id]
		if job == nil then
			return { ok = false, error = "Creator job wasn't found" }
		end
		return {
			ok = true,
			jobId = id,
			status = job.status,
			result = job.result,
			error = job.error,
			elapsedSeconds = (job.finishedAt or os.clock()) - job.startedAt,
		}
	end

	function api.cleanup()
		for _, job in pairs(jobs) do
			if job.status == "running" and coroutine.status(job.thread) ~= "dead" then
				task.cancel(job.thread)
			end
		end
		table.clear(jobs)
		table.clear(jobOrder)
		for token, state in pairs(cameraStates) do
			cameraStates[token] = nil
			state.camera.CameraType = state.cameraType
			state.camera.CFrame = state.cframe
			state.camera.Focus = state.focus
		end
	end

	return api
end

return BridgeCreatorApi
