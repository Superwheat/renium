local roblox = require("@lune/roblox")

game = roblox.Instance.new("DataModel")

local Chunking = require("../BridgeChunking.module")
local Content = require("../BridgeContent.module")
local Identity = require("../BridgeIdentity.module")
local Settings = require("../BridgeSettings.module")

local function equal(actual, expected, label)
	if actual ~= expected then
		error(string.format("%s: expected %s, got %s", label, tostring(expected), tostring(actual)))
	end
end

local chunk = Chunking.chunkEncodedString("abcdefgh", 3, 3, -2)
equal(chunk.start, 3, "chunk start")
equal(chunk.nextStart, 6, "chunk next start")
equal(chunk.total, 8, "chunk total")
equal(chunk.chunk, "cde", "chunk body")
equal(chunk.pluginEncodeMs, 0, "chunk timing")

local exhausted = Chunking.chunkEncodedString("abc", 9, 4, 1)
equal(exhausted.chunk, "", "exhausted body")
equal(exhausted.nextStart, 9, "exhausted next start")

local cache = { first = "a", second = "b" }
local cacheKeys = { "first", "second" }
Chunking.removeCachedPayload(cache, cacheKeys, "first")
equal(cache.first, nil, "cache removal")
equal(cacheKeys[1], "second", "cache key removal")

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

local segments = Identity.getRefPathSegments(child)
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

local hash = Identity.shortenIdentifier("renium")
equal(#hash, 16, "short identifier length")
equal(hash, Identity.shortenIdentifier("renium"), "short identifier stability")

equal(Content.serializeSource("None", nil), "", "empty content")
equal(Content.serializeSource("Uri", "rbxassetid://123"), "rbxassetid://123", "URI content")
local objectOk, objectError = pcall(Content.serializeSource, "Object", nil)
equal(objectOk, false, "object content rejection")
equal(string.find(objectError, "without losing data", 1, true) ~= nil, true, "object content error")
local opaqueOk = pcall(Content.serializeSource, "Opaque", nil)
equal(opaqueOk, false, "opaque content rejection")

print("Renium plugin behavior tests passed")
