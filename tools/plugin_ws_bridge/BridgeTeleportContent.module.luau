local Content = {}
local SCRIPT_STATE = "__ReniumTeleportDisabled"
local REF_ID = "__ReniumTeleportRef"
local SERVICES = { Workspace = true, Lighting = true, ReplicatedFirst = true, ReplicatedStorage = true,
	ServerScriptService = true, ServerStorage = true, StarterGui = true, StarterPack = true,
	StarterPlayer = true, Teams = true, SoundService = true }
local DEFAULT_SCRIPTS = { PlayerModule = true, RbxCharacterSounds = true, CameraScript = true, ControlScript = true }

local function executable(node)
	return node:IsA("Script") or node:IsA("LocalScript")
end

local function target(path)
	assert(type(path) == "table" and SERVICES[path[1]], "Unsupported teleport service")
	assert(#path == 1 or #path == 2 and path[1] == "StarterPlayer"
		and (path[2] == "StarterPlayerScripts" or path[2] == "StarterCharacterScripts"), "Invalid teleport container")
	local service = game:GetService(path[1])
	return if #path == 1 then service else assert(service:FindFirstChild(path[2]), "StarterPlayer container is missing")
end

local function enable(root)
	local nodes, scripts = root:GetDescendants(), {}
	nodes[#nodes + 1] = root
	for _, node in nodes do
		if executable(node) then
			local disabled = node:GetAttribute(SCRIPT_STATE)
			if disabled ~= nil then
				scripts[#scripts + 1] = { node = node, disabled = disabled }
				node:SetAttribute(SCRIPT_STATE, nil)
			end
		end
		node:SetAttribute(REF_ID, nil)
	end
	for _, row in scripts do
		row.node.Disabled = row.disabled
	end
end

local function propertiesFor(destination, rows, codec, collisionGroups)
	local properties = {}
	for _, row in rows do
		local ok, value = codec.decode(row.value, nil, function()
			error("Unsupported container reference")
		end)
		assert(ok, `Unsupported teleport property {row.name}`)
		if row.name == "CollisionGroupData" and destination == workspace then
			collisionGroups.decode(value)
			properties[#properties + 1] = { name = row.name, value = value, collisionGroups = collisionGroups }
		else
			-- Protected getters can be inaccessible in the selected Play runtime.
			local readable, current = pcall(function()
				return destination[row.name]
			end)
			assert(readable, `Teleport cannot read {destination.Name}.{row.name}`)
			if row.write then
				properties[#properties + 1] = { name = row.name, value = value }
			else
				assert(current == value, `Teleport cannot change {destination.Name}.{row.name} during Play`)
			end
		end
	end
	return properties
end

function Content.prepare(roots, manifest, codec, collisionGroups)
	assert(#roots == #manifest.groups, "Teleport payload group count differs")
	for _, check in manifest.checks do
		local destination = game:GetService(check.path[1])
		for index = 2, #check.path do
			destination = assert(destination:FindFirstChild(check.path[index]), "Retained container is missing")
		end
		assert(destination.ClassName == check.class, "Retained service class changed")
		assert(not check.empty or #destination:GetChildren() == 0, "Unsupported source content in retained service")
		assert(next(destination:GetAttributes()) == nil and #destination:GetTags() == 0, "Retained service metadata differs")
		propertiesFor(destination, check.properties, codec, collisionGroups)
	end
	local byName, byId = {}, {}
	for _, root in roots do
		assert(root:IsA("Folder") and root.Parent == nil and not byName[root.Name], "Invalid teleport payload root")
		byName[root.Name] = root
		for _, node in root:GetDescendants() do
			assert(not node:IsA("Terrain") and not node:IsA("PackageLink"), "Unsupported teleport content")
			assert(not executable(node) or node.Disabled and type(node:GetAttribute(SCRIPT_STATE)) == "boolean", "Teleport payload has an enabled or unstaged script")
			local id = node:GetAttribute(REF_ID)
			assert(type(id) == "string" and not byId[id], "Invalid teleport payload identity")
			byId[id] = node
		end
	end
	local prepared, used = {}, {}
	for _, group in manifest.groups do
		local destination = target(group.path)
		assert(not used[destination], "Duplicate teleport container")
		used[destination] = true
		local root = assert(byName[group.name], "Teleport payload group is missing")
		if group.path[2] == "StarterPlayerScripts" then
			for name in DEFAULT_SCRIPTS do
				assert(not destination:FindFirstChild(name) or root:FindFirstChild(name), `Teleport cannot restore an engine controller overridden by the source: {name}`)
			end
		end
		byName[group.name] = nil
		local properties = propertiesFor(destination, group.properties, codec, collisionGroups)
		prepared[#prepared + 1] = { root = root, target = destination, properties = properties }
	end
	for _, row in manifest.references do
		local node = assert(byId[row.id], "Teleport reference owner is missing")
		node[row.property] = target(row.path)
	end
	return prepared
end

local function clearContainer(container, retained, nativeRelay)
	for _, child in container:GetChildren() do
		if child == retained or child == nativeRelay then
			continue
		elseif child:IsA("Terrain") then
			child:Clear()
		elseif child ~= workspace.CurrentCamera and not child:IsA("StarterPlayerScripts") and not child:IsA("StarterCharacterScripts") then
			child:Destroy()
		end
	end
end

function Content.installShim(template, request)
	local storage = game:GetService("ReplicatedStorage")
	assert(not storage:FindFirstChild("ReniumTeleport"), "ReplicatedStorage.ReniumTeleport is already used by the game; compatibility teleports are unavailable")
	local shim = template:Clone()
	shim.Name = "ReniumTeleport"
	local context = Instance.new("StringValue")
	context.Name = "Context"
	context.Value = game:GetService("HttpService"):JSONEncode({ mode = "source", placeId = game.PlaceId,
		originPlaceId = game.PlaceId, sourcePlaceId = game.PlaceId, gameId = game.GameId, jobId = game.JobId, data = {} })
	context.Parent = shim
	local invoke = Instance.new("BindableFunction")
	invoke.Name = "Request"
	invoke.OnInvoke = request
	invoke.Parent = shim
	shim.Parent = storage
	return shim
end

local function copyChildren(source, destination)
	local originals = source:GetDescendants()
	originals[#originals + 1] = source
	for _, node in originals do
		assert(node:GetAttribute("__ReniumTeleportArchivable") == nil, "Reserved teleport clone attribute")
		node:SetAttribute("__ReniumTeleportArchivable", node.Archivable)
		node.Archivable = true
	end
	-- Clone the complete group to retain references between siblings.
	local copy = source:Clone()
	for _, node in originals do
		node.Archivable = node:GetAttribute("__ReniumTeleportArchivable")
		node:SetAttribute("__ReniumTeleportArchivable", nil)
	end
	assert(copy, `Cannot copy {source.Name} for teleport clients`)
	local copies = copy:GetDescendants()
	copies[#copies + 1] = copy
	for _, node in copies do
		node.Archivable = node:GetAttribute("__ReniumTeleportArchivable")
		node:SetAttribute("__ReniumTeleportArchivable", nil)
	end
	for _, child in copy:GetChildren() do
		child.Parent = destination
	end
	copy:Destroy()
end

function Content.stageClients(prepared, params, shimTemplate)
	local shim = shimTemplate:Clone()
	shim.Name = "ReniumTeleport"
	local context = Instance.new("StringValue")
	context.Name = "Context"
	context.Value = game:GetService("HttpService"):JSONEncode({
		mode = "emulated", id = params.id, placeId = params.placeId, sourcePlaceId = params.sourcePlaceId,
		originPlaceId = params.originPlaceId,
		gameId = params.gameId, version = params.version, contentSource = params.contentSource, data = params.data, jobId = game.JobId,
	})
	context.Parent = shim
	local metadata = Instance.new("Folder")
	metadata.Name = "ServiceMetadata"
	metadata.Parent = shim
	for _, group in prepared do
		local settings = Instance.new("Folder")
		settings.Name = group.root.Name
		for key, value in group.root:GetAttributes() do
			if key ~= REF_ID then
				settings:SetAttribute(key, value)
			end
		end
		for _, tag in group.root:GetTags() do
			settings:AddTag(tag)
		end
		settings.Parent = metadata
		local name = if group.target == game:GetService("StarterPlayer").StarterPlayerScripts then "PlayerScripts"
			else if group.target == game:GetService("ReplicatedFirst") then "First"
			else if group.target == game:GetService("StarterGui") then "Gui" else nil
		if name then
			local folder = Instance.new("Folder")
			folder.Name = name
			copyChildren(group.root, folder)
			folder.Parent = shim
		end
	end
	shim:SetAttribute("Attempt", params.id)
	return shim
end

function Content.commit(prepared, params, bundle, guard, shim, nativeRelay)
	local Players = game:GetService("Players")
	for _, group in prepared do
		for _, node in group.target:GetDescendants() do
			if executable(node) then
				node.Disabled = true
			end
		end
	end
	for _, player in Players:GetPlayers() do
		if player.Character then
			player.Character:Destroy()
		end
		player.Backpack:ClearAllChildren()
		player.StarterGear:ClearAllChildren()
	end
	for _, group in prepared do
		guard()
		clearContainer(group.target, shim, nativeRelay)
	end
	for _, group in prepared do
		for _, row in group.properties do
			if row.collisionGroups then
				row.collisionGroups.write(row.value)
			else
				group.target[row.name] = row.value
			end
		end
		for key in group.target:GetAttributes() do
			group.target:SetAttribute(key, nil)
		end
		for key, value in group.root:GetAttributes() do
			if key ~= REF_ID then
				group.target:SetAttribute(key, value)
			end
		end
		for _, tag in group.target:GetTags() do
			group.target:RemoveTag(tag)
		end
		for _, tag in group.root:GetTags() do
			group.target:AddTag(tag)
		end
		for _, child in group.root:GetChildren() do
			child.Parent = group.target
		end
	end
	-- Preserve the module and Request identity across hops, including cached require results.
	for _, child in shim:GetChildren() do
		if child.Name ~= "Request" and child.Name ~= "Context" then
			child:Destroy()
		end
	end
	shim.Context.Value = bundle.Context.Value
	bundle.Context:Destroy()
	for _, child in bundle:GetChildren() do
		child.Parent = shim
	end
	shim:SetAttribute("Attempt", params.id)
	local count = #shim:GetDescendants()
	for _, group in prepared do
		-- Starter scripts retain their disabled marker until each client copies them.
		if group.target ~= game:GetService("StarterPlayer").StarterPlayerScripts
			and group.target ~= game:GetService("StarterGui") and group.target ~= game:GetService("ReplicatedFirst")
			and group.target ~= game:GetService("ReplicatedStorage") then
			enable(group.target)
		end
	end
	-- The transport bundle must stay disabled in ReplicatedStorage.
	for _, child in game:GetService("ReplicatedStorage"):GetChildren() do
		if child ~= shim then
			enable(child)
		end
	end
	for _, player in Players:GetPlayers() do
		guard()
		player:LoadCharacterAsync()
	end
	return count
end

function Content.finishTemplates()
	enable(game:GetService("StarterGui"))
	enable(game:GetService("StarterPlayer").StarterPlayerScripts)
end

function Content.pauseClient(gui)
	local player = game:GetService("Players").LocalPlayer
	local paused = {}
	for _, container in { player.PlayerScripts, player.PlayerGui, player.Backpack } do
		for _, node in container:GetDescendants() do
			if executable(node) and not node:IsDescendantOf(gui) then
				paused[#paused + 1] = { node = node, disabled = node.Disabled }
				node.Disabled = true
			end
		end
	end
	return paused
end

function Content.prepareClient(groups, codec, collisionGroups)
	local prepared = {}
	for _, group in groups do
		if group.path[1] ~= "ServerStorage" and group.path[1] ~= "ServerScriptService" then
			local rows = {}
			for _, row in group.properties do
				-- Collision groups are server-owned and replicate through PhysicsService.
				if row.name ~= "CollisionGroupData" then
					table.insert(rows, row)
				end
			end
			local destination = target(group.path)
			table.insert(prepared, { target = destination, name = group.name,
				properties = propertiesFor(destination, rows, codec, collisionGroups) })
		end
	end
	return prepared
end

function Content.clearClient(gui, prepared)
	-- Client-created children do not disappear when the server replaces its copies.
	-- Run at the irreversible boundary, before destination replication can arrive.
	local shim = game:GetService("ReplicatedStorage"):FindFirstChild("ReniumTeleport")
	local arriving = shim and shim:FindFirstChild("ArrivingGui")
	if arriving then
		-- This client-only receipt is absent from the next server descendant count.
		arriving:Destroy()
	end
	for service in SERVICES do
		if service ~= "ServerStorage" and service ~= "ServerScriptService" then
			local container = game:GetService(service)
			clearContainer(container, shim)
			if service == "StarterPlayer" then
				clearContainer(container.StarterPlayerScripts)
				clearContainer(container.StarterCharacterScripts)
			end
		end
	end
	local player = game:GetService("Players").LocalPlayer
	for _, child in player.PlayerScripts:GetChildren() do
		if not DEFAULT_SCRIPTS[child.Name] then
			child:Destroy()
		end
	end
	for _, child in player.PlayerGui:GetChildren() do
		if child ~= gui then
			child:Destroy()
		end
	end
	player.Backpack:ClearAllChildren()
	for _, group in prepared do
		for key in group.target:GetAttributes() do
			group.target:SetAttribute(key, nil)
		end
		for _, tag in group.target:GetTags() do
			group.target:RemoveTag(tag)
		end
		for _, row in group.properties do
			group.target[row.name] = row.value
		end
	end
end

function Content.resumeClient(paused)
	for _, row in paused do
		if row.node.Parent then
			row.node.Disabled = row.disabled
		end
	end
end

function Content.arriveClient(params, gui, guard, prepared)
	local storage = game:GetService("ReplicatedStorage")
	local deadline = os.clock() + 30
	local shim
	repeat
		guard()
		shim = storage:FindFirstChild("ReniumTeleport")
		if shim and shim:GetAttribute("Attempt") == params.id and #shim:GetDescendants() == params.replicatedCount then
			break
		end
		assert(os.clock() < deadline, "Destination client content did not finish replicating; Stop Play")
		task.wait()
	until false
	guard()
	for _, group in prepared do
		local metadata = assert(shim.ServiceMetadata:FindFirstChild(group.name), "Destination service metadata is missing")
		for key, value in metadata:GetAttributes() do
			group.target:SetAttribute(key, value)
		end
		for _, tag in metadata:GetTags() do
			group.target:AddTag(tag)
		end
	end
	local player = game:GetService("Players").LocalPlayer
	local scripts = player.PlayerScripts
	local incoming = shim.PlayerScripts
	-- Keep Roblox's default controllers unless the destination supplies replacements.
	for _, child in scripts:GetChildren() do
		if incoming:FindFirstChild(child.Name) or not DEFAULT_SCRIPTS[child.Name] then
			child:Destroy()
		end
	end
	for _, child in player.PlayerGui:GetChildren() do
		if child ~= gui then
			child:Destroy()
		end
	end
	clearContainer(game:GetService("ReplicatedFirst"))
	copyChildren(incoming, scripts)
	copyChildren(shim.Gui, player.PlayerGui)
	copyChildren(shim.First, game:GetService("ReplicatedFirst"))
	local arriving = Instance.new("ObjectValue")
	arriving.Name = "ArrivingGui"
	arriving.Value = gui
	arriving.Parent = shim
	enable(scripts)
	enable(player.PlayerGui)
	enable(game:GetService("ReplicatedFirst"))
end

return Content
