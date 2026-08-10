local BridgeIdentity = require(script.Parent.BridgeIdentity)
local ScriptEditorService = game:GetService("ScriptEditorService")

local BridgeScriptDocuments = {}

local function findScriptDocument(instance: Instance): any?
	local ok, document = pcall((ScriptEditorService :: any).FindScriptDocument, ScriptEditorService, instance)
	if ok and document ~= nil then
		return document
	end
	return nil
end

function BridgeScriptDocuments.readSource(instance: Instance): (boolean, any)
	local okEditor, editorSource = pcall((ScriptEditorService :: any).GetEditorSource, ScriptEditorService, instance)
	if okEditor then
		return true, editorSource
	end
	return pcall(function()
		return (instance :: any).Source
	end)
end

local function documentLineEndCharacter(document: any, line: number): number
	local okLine, lineText = pcall(document.GetLine, document, line)
	if okLine and type(lineText) == "string" then
		return #lineText + 1
	end
	return 1
end

local function clampDocumentPosition(document: any, line: any, character: any): (number, number)
	local okCount, lineCount = pcall(document.GetLineCount, document)
	if not okCount or type(lineCount) ~= "number" or lineCount < 1 then
		return 1, 1
	end
	local clampedLine = math.clamp(math.floor(tonumber(line) or 1), 1, lineCount)
	local lineEnd = documentLineEndCharacter(document, clampedLine)
	local clampedCharacter = math.clamp(math.floor(tonumber(character) or 1), 1, lineEnd)
	return clampedLine, clampedCharacter
end

local function getDocumentSelection(document: any): { number }?
	local okSelection, cursorLine, cursorCharacter, anchorLine, anchorCharacter = pcall(document.GetSelection, document)
	if not okSelection or type(cursorLine) ~= "number" or type(cursorCharacter) ~= "number" then
		return nil
	end
	local resolvedAnchorLine = if type(anchorLine) == "number" then anchorLine else cursorLine
	local resolvedAnchorCharacter = if type(anchorCharacter) == "number" then anchorCharacter else cursorCharacter
	return { cursorLine, cursorCharacter, resolvedAnchorLine, resolvedAnchorCharacter }
end

local function restoreDocumentSelection(document: any, selection: { number }?)
	if selection == nil then
		return
	end
	local cursorLine, cursorCharacter = clampDocumentPosition(document, selection[1], selection[2])
	local anchorLine, anchorCharacter = clampDocumentPosition(document, selection[3], selection[4])
	local okRequest, success =
		pcall(document.RequestSetSelectionAsync, document, cursorLine, cursorCharacter, anchorLine, anchorCharacter)
	if okRequest and success ~= false then
		return
	end
	pcall(document.ForceSetSelectionAsync, document, cursorLine, cursorCharacter, anchorLine, anchorCharacter)
end

local function setOpenDocumentSource(document: any, source: string): (boolean, any)
	local okText, currentText = pcall(document.GetText, document)
	if okText and currentText == source then
		return true, nil
	end

	local okLineCount, lineCount = pcall(document.GetLineCount, document)
	if not okLineCount or type(lineCount) ~= "number" or lineCount < 1 then
		return false, "open script document did not report a valid line count"
	end

	local selection = getDocumentSelection(document)
	local endCharacter = documentLineEndCharacter(document, lineCount)
	local okEdit, success, editErr = pcall(document.EditTextAsync, document, source, 1, 1, lineCount, endCharacter)
	if not okEdit then
		return false, success
	end
	if success == false then
		return false, editErr or "EditTextAsync failed"
	end
	restoreDocumentSelection(document, selection)
	return true, nil
end

function BridgeScriptDocuments.setSource(
	instance: Instance,
	source: string,
	ctx: { [string]: any }?
): (boolean, any, string)
	local document = findScriptDocument(instance)
	local token = if instance:IsDescendantOf(game) and ctx ~= nil
		then ctx.expectPropertyEvent(instance, "Source", source)
		else nil
	if document ~= nil then
		local documentOk = setOpenDocumentSource(document, source)
		if documentOk then
			return true, nil, "ScriptDocument"
		end
	end

	local updateOk = pcall(function()
		(ScriptEditorService :: any):UpdateSourceAsync(instance, function()
			return source
		end)
	end)
	if updateOk then
		return true, nil, "UpdateSourceAsync"
	end
	local ok, err = pcall(function()
		(instance :: any).Source = source
	end)
	if ok then
		return true, nil, "Source"
	end
	if token ~= nil and ctx ~= nil then
		ctx.cancelExpectedEvent(token)
	end
	return false, err, "Source"
end

local function readInstanceDebugId(instance: Instance): string?
	local ok, debugId = pcall(instance.GetDebugId, instance, 32)
	return if ok and type(debugId) == "string" and debugId ~= "" then debugId else nil
end

function BridgeScriptDocuments.capture(serviceNames: { string }, includedKeys: { [string]: boolean }?): { any }
	local includedServices = {}
	for _, serviceName in ipairs(serviceNames) do
		includedServices[serviceName] = true
	end
	local entries = {}
	for _, document in ipairs(ScriptEditorService:GetScriptDocuments()) do
		if not document:IsCommandBar() then
			local scriptInstance = document:GetScript()
			local pathSegments, pathOrdinals = BridgeIdentity.getRefPathParts(scriptInstance)
			local key = if pathSegments ~= nil then BridgeIdentity.pathCacheKey(pathSegments, pathOrdinals) else nil
			if
				pathSegments ~= nil
				and includedServices[pathSegments[1]]
				and (includedKeys == nil or includedKeys[key])
			then
				local source = document:GetText()
				local okStored, storedSource = pcall(function()
					return (scriptInstance :: any).Source
				end)
				entries[#entries + 1] = {
					document = document,
					instance = scriptInstance,
					pathSegments = pathSegments,
					pathOrdinals = pathOrdinals,
					key = key,
					debugId = readInstanceDebugId(scriptInstance),
					source = source,
					dirty = not okStored or storedSource ~= source,
					selection = getDocumentSelection(document),
				}
			end
		end
	end
	return entries
end

function BridgeScriptDocuments.apply(
	entries: { any },
	changedSourceInstances: { [Instance]: boolean }?,
	changedSourceKeys: { [string]: boolean }?,
	replacements: { [Instance]: Instance }?,
	resolveStagedPath: (({ string }, { number }?) -> Instance?)?,
	allSourcesChanged: boolean?
)
	for _, entry in ipairs(entries) do
		local target = entry.instance
		local seen = {}
		while replacements ~= nil and replacements[target] ~= nil and not seen[target] do
			seen[target] = true
			target = replacements[target]
		end
		if BridgeIdentity.liveInstance(target) == nil then
			target = nil
		end
		if target == nil and resolveStagedPath ~= nil then
			target = resolveStagedPath(entry.pathSegments, entry.pathOrdinals)
		end
		if target == nil and entry.debugId ~= nil then
			local pathTarget = BridgeIdentity.resolvePathSegments(entry.pathSegments, nil, entry.pathOrdinals)
			if pathTarget ~= nil and readInstanceDebugId(pathTarget) == entry.debugId then
				target = pathTarget
			end
		end
		if (target == nil or not target:IsA("LuaSourceContainer")) and allSourcesChanged then
			local okClose, closed, closeError = pcall(entry.document.CloseAsync, entry.document)
			if not okClose or closed == false then
				error(`Could not close removed script document: {closeError or closed}`)
			end
			continue
		end
		if target == nil or not target:IsA("LuaSourceContainer") then
			error("Could not restore open script document " .. table.concat(entry.pathSegments, "."))
		end
		local sourceChanged = allSourcesChanged or (changedSourceKeys ~= nil and changedSourceKeys[entry.key])
		if changedSourceInstances ~= nil then
			sourceChanged = sourceChanged or changedSourceInstances[entry.instance] or changedSourceInstances[target]
		end
		local source = entry.source
		local selection = entry.selection
		if not sourceChanged then
			local okCurrentSource, currentSource = pcall(entry.document.GetText, entry.document)
			if okCurrentSource then
				source = currentSource
				selection = getDocumentSelection(entry.document)
			end
		end
		local targetDocument = findScriptDocument(target)
		if targetDocument == nil then
			local okOpen, opened, openError =
				pcall(ScriptEditorService.OpenScriptDocumentAsync, ScriptEditorService, target)
			if not okOpen or opened == false then
				error(`Could not open replacement script document {target:GetFullName()}: {openError or opened}`)
			end
			targetDocument = findScriptDocument(target)
			if targetDocument == nil then
				error("Replacement script document did not open for " .. target:GetFullName())
			end
		end
		if not sourceChanged and targetDocument ~= entry.document then
			local okWrite, writeError = setOpenDocumentSource(targetDocument, source)
			if not okWrite then
				error(`Could not restore open script document {target:GetFullName()}: {writeError}`)
			end
		end
		restoreDocumentSelection(targetDocument, selection)
		if entry.document ~= targetDocument then
			local okClose, closed, closeError = pcall(entry.document.CloseAsync, entry.document)
			if not okClose or closed == false then
				error(`Could not close replaced script document: {closeError or closed}`)
			end
		end
	end
end

return BridgeScriptDocuments
