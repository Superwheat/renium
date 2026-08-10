local roblox = require("@lune/roblox")

game = roblox.Instance.new("DataModel")

local CandidateMatch = require("../BridgeCandidateMatch.module")
local Chunking = require("../BridgeChunking.module")
local Content = require("../BridgeContent.module")
local Identity = require("../BridgeIdentity.module")
local InstanceSwap = require("../BridgeInstanceSwap.module")
local ReferenceRetarget = require("../BridgeReferenceRetarget.module")
local Settings = require("../BridgeSettings.module")
local ValueEquality = require("../BridgeValueEquality.module")

local function equal(actual, expected, label)
	if actual ~= expected then
		error(string.format("%s: expected %s, got %s", label, tostring(expected), tostring(actual)))
	end
end

local Runtime = require("../BridgePluginRuntime.module")
equal(type(Runtime.start), "function", "runtime module compilation")

local chunk = Chunking.chunkEncodedString("abcdefgh", 3, 3, -2)
equal(chunk.start, 3, "chunk start")
equal(chunk.nextStart, 6, "chunk next start")
equal(chunk.total, 8, "chunk total")
equal(chunk.chunk, "cde", "chunk body")
equal(chunk.pluginEncodeMs, 0, "chunk timing")

local exhausted = Chunking.chunkEncodedString("abc", 9, 4, 1)
equal(exhausted.chunk, "", "exhausted body")
equal(exhausted.nextStart, 9, "exhausted next start")

equal(Settings.normalizeLoopbackHost(" localhost "), "127.0.0.1", "localhost normalization")
equal(Settings.normalizeLoopbackHost("[::1]"), "::1", "IPv6 normalization")
equal(Settings.normalizeLoopbackHost("0.0.0.0"), nil, "remote host rejection")
equal(Settings.formatWebSocketUrl("::1", 8781), "ws://[::1]:8781", "IPv6 URL")

local ports = Settings.parsePortsCsv("8781, 8782,8781")
equal(#ports, 2, "port deduplication")
equal(ports[1], 8781, "first port")
equal(ports[2], 8782, "second port")
equal(Settings.parsePortsCsv("0,8781"), nil, "invalid port rejection")
equal(Settings.normalizeRuntimeSetting("changesThreshold", -3), 0, "number minimum")
equal(Settings.normalizeRuntimeSetting("diffLinesLimit", 2000000), 1000000, "number maximum")
equal(Settings.normalizeRuntimeSetting("initialSyncPriority", " EDITOR "), "editor", "enum normalization")
equal(Settings.normalizeRuntimeSetting("initialSyncPriority", "bad"), "studio", "enum fallback")

local workspace = game:GetService("Workspace")
local folder = roblox.Instance.new("Folder")
folder.Name = "Parent"
folder.Parent = workspace
local child = roblox.Instance.new("Folder")
child.Name = "Child"
child.Parent = folder

local segments = Identity.getRefPathParts(child)
equal(#segments, 3, "reference path length")
equal(segments[1], "Workspace", "reference service")
equal(segments[2], "Parent", "reference parent")
equal(segments[3], "Child", "reference child")

local state = {
	instanceIdByInstance = { [child] = 42 },
	pathSegmentsByInstance = {},
	debugIdByInstance = {},
}
local reference = Identity.serializeRefValue(state, child)
equal(reference._type, "Ref", "reference type")
equal(reference.instanceId, "2a", "reference id")

equal(ValueEquality.valuesEqual(1, 1.00005), true, "number epsilon")
equal(ValueEquality.valuesEqual(1, 1.001), false, "number difference")
equal(ValueEquality.valuesEqual(0 / 0, 0 / 0), true, "NaN equality")
equal(
	ValueEquality.valuesEqual(roblox.Vector3.new(1, 2, 3), roblox.Vector3.new(1.00005, 2, 3)),
	true,
	"Vector3 epsilon"
)
equal(ValueEquality.valuesEqual(roblox.CFrame.new(1, 2, 3), roblox.CFrame.new(1.00005, 2, 3)), true, "CFrame epsilon")
equal(
	ValueEquality.valuesEqual(roblox.Color3.new(0.5, 0.25, 0.75), roblox.Color3.new(0.50001, 0.25, 0.75)),
	true,
	"Color3 quantization"
)
equal(
	ValueEquality.valuesEqual(roblox.UDim2.new(0.5, 2, 0.25, 4), roblox.UDim2.new(0.50005, 2, 0.25, 4)),
	true,
	"UDim2 epsilon"
)
equal(
	ValueEquality.valuesEqual(roblox.NumberRange.new(1, 2), roblox.NumberRange.new(1.00005, 2)),
	true,
	"NumberRange epsilon"
)
equal(
	ValueEquality.valuesEqual(
		roblox.PhysicalProperties.new(0.7, 0.3, 0.5, 1, 1),
		roblox.PhysicalProperties.new(0.70005, 0.3, 0.5, 1, 1)
	),
	true,
	"PhysicalProperties epsilon"
)
equal(
	ValueEquality.valuesEqual(
		roblox.NumberSequence.new({
			roblox.NumberSequenceKeypoint.new(0, 0, 0),
			roblox.NumberSequenceKeypoint.new(1, 1, 0),
		}),
		roblox.NumberSequence.new({
			roblox.NumberSequenceKeypoint.new(0, 0.00005, 0),
			roblox.NumberSequenceKeypoint.new(1, 1, 0),
		})
	),
	true,
	"NumberSequence epsilon"
)
equal(ValueEquality.valuesEqual({ value = 1 }, { value = 1.00005 }), true, "table epsilon")

local matchCandidates = {
	{ Value = "first", Marker = "one" },
	{ Value = "second", Marker = "two" },
}
local matchedCandidate = CandidateMatch.choose(
	matchCandidates,
	{ Value = "second" },
	{ Marker = "two" },
	function(candidate, name, value)
		return candidate[name] == value
	end,
	function(candidate, name, value)
		return candidate[name] == value
	end
)
equal(matchedCandidate, matchCandidates[2], "candidate property scoring")

local tiedCandidate = CandidateMatch.choose(
	matchCandidates,
	{ Target = { _type = "Ref", settingsId = "second" } },
	{},
	function(candidate, name, value)
		return candidate[name] == value
	end,
	function(candidate, name, value)
		return candidate[name] == value
	end
)
equal(tiedCandidate, matchCandidates[1], "candidate reference tie")

local oversizedCandidates = {}
for index = 1, 33 do
	oversizedCandidates[index] = { Value = index }
end
local cappedCandidate = CandidateMatch.choose(oversizedCandidates, { Value = 33 }, {}, function(candidate, name, value)
	return candidate[name] == value
end, function(candidate, name, value)
	return candidate[name] == value
end)
equal(cappedCandidate, oversizedCandidates[1], "candidate scoring cap")

local FakeInstance = {}
local FakeInstanceMetatable = {}
function FakeInstanceMetatable.__index(instance, key)
	if key == "Parent" then
		return rawget(instance, "_parent")
	end
	return FakeInstance[key] or rawget(instance, key)
end
function FakeInstanceMetatable.__newindex(instance, key, value)
	if key ~= "Parent" then
		rawset(instance, key, value)
		return
	end
	local parent = rawget(instance, "_parent")
	if parent ~= nil then
		table.remove(parent._children, table.find(parent._children, instance))
	end
	rawset(instance, "_parent", value)
	if value ~= nil then
		value._children[#value._children + 1] = instance
	end
end
function FakeInstance.new(className)
	return setmetatable({
		ClassName = className,
		Name = className,
		_attributes = {},
		_children = {},
	}, FakeInstanceMetatable)
end
function FakeInstance:GetChildren()
	return table.clone(self._children)
end
function FakeInstance:GetDescendants()
	local descendants = {}
	local function visit(instance)
		for _, childInstance in ipairs(instance:GetChildren()) do
			descendants[#descendants + 1] = childInstance
			visit(childInstance)
		end
	end
	visit(self)
	return descendants
end
function FakeInstance:GetAttributes()
	return table.clone(self._attributes)
end
function FakeInstance:SetAttribute(name, value)
	self._attributes[name] = value
end
function FakeInstance:GetAttribute(name)
	return self._attributes[name]
end
function FakeInstance:GetFullName()
	return self.Name
end
function FakeInstance:Destroy()
	self.Parent = nil
end

local tagsByInstance = {}
local tagService = {}
function tagService:GetTags(instance)
	return table.clone(tagsByInstance[instance] or {})
end
function tagService:AddTag(instance, tag)
	local tags = tagsByInstance[instance] or {}
	tagsByInstance[instance] = tags
	tags[#tags + 1] = tag
end

local swapParent = FakeInstance.new("Folder")
local before = FakeInstance.new("Folder")
before.Name = "Value"
before.Parent = swapParent
local replaced = FakeInstance.new("Folder")
replaced.Name = "Value"
replaced:SetAttribute("Marker", "middle")
replaced.Parent = swapParent
tagsByInstance[replaced] = { "Synced" }
local nested = FakeInstance.new("Folder")
nested.Name = "Nested"
nested.Parent = replaced
local nestedTail = FakeInstance.new("Folder")
nestedTail.Name = "NestedTail"
nestedTail.Parent = replaced
local after = FakeInstance.new("Folder")
after.Name = "Value"
after.Parent = swapParent

local replacement = InstanceSwap.replace(replaced, "StringValue", tagService, function(instance)
	instance.Parent = nil
end, FakeInstance.new)
local swappedChildren = swapParent:GetChildren()
equal(swappedChildren[1], before, "replacement first sibling")
equal(swappedChildren[2], replacement, "replacement original position")
equal(swappedChildren[3], after, "replacement trailing sibling")
equal(nested.Parent, replacement, "replacement child parent")
equal(nestedTail.Parent, replacement, "replacement trailing child parent")
equal(replacement:GetAttribute("Marker"), "middle", "replacement attribute")
equal(tagsByInstance[replacement][1], "Synced", "replacement tag")

local nestedReplacement = InstanceSwap.replace(nested, "StringValue", tagService, function(instance)
	instance.Parent = nil
end, FakeInstance.new)
local nestedChildren = replacement:GetChildren()
equal(nestedChildren[1], nestedReplacement, "nested replacement position")
equal(nestedChildren[2], nestedTail, "nested replacement trailing sibling")

local referenceHolder = FakeInstance.new("ObjectValue")
referenceHolder.Reference = replaced
referenceHolder.Parent = swapParent
local referenceUpdated, referenceFailed = ReferenceRetarget.apply(
	{ swapParent },
	{ [replaced] = replacement },
	function(className)
		return if className == "ObjectValue" then { "Reference" } else {}
	end,
	function(instance, propertyName)
		return true, instance[propertyName]
	end,
	function(instance, propertyName, value)
		instance[propertyName] = value
		return true
	end
)
equal(referenceUpdated, 1, "replacement reference update")
equal(referenceFailed, 0, "replacement reference failure count")
equal(referenceHolder.Reference, replacement, "replacement reference target")

local lockedParent = FakeInstance.new("Folder")
local lockedTarget = FakeInstance.new("Folder")
lockedTarget.Parent = lockedParent
local lockedSibling = FakeInstance.new("Terrain")
lockedSibling.RobloxLocked = true
lockedSibling.Parent = lockedParent
local lockedReplacementOk = pcall(InstanceSwap.replace, lockedTarget, "StringValue", tagService, function(instance)
	instance.Parent = nil
end, FakeInstance.new)
local lockedChildren = lockedParent:GetChildren()
equal(lockedReplacementOk, false, "locked sibling replacement rejected")
equal(lockedChildren[1], lockedTarget, "locked sibling target retained")
equal(lockedChildren[2], lockedSibling, "locked sibling retained")

equal(Content.serializeSource("None", nil), "", "empty content")
equal(Content.serializeSource("Uri", "rbxassetid://123"), "rbxassetid://123", "URI content")
local objectOk, objectError = pcall(Content.serializeSource, "Object", nil)
equal(objectOk, false, "object content rejection")
equal(string.find(objectError, "without losing data", 1, true) ~= nil, true, "object content error")
local opaqueOk = pcall(Content.serializeSource, "Opaque", nil)
equal(opaqueOk, false, "opaque content rejection")

print("Renium plugin behavior tests passed")
