local fs = require("@lune/fs")
local luau = require("@lune/luau")
local source = fs.readFile("tools/plugin_ws_bridge/BridgeEditorSync.module.luau")

local function node(name, service)
	return {
		Name = name,
		service = service,
		IsDescendantOf = function(self, ancestor)
			return self.service == ancestor
		end,
	}
end

local services = { ReplicatedStorage = { Name = "ReplicatedStorage" }, Workspace = { Name = "Workspace" } }
local renamed = node("Renamed", services.ReplicatedStorage)
local squatter = node("A", services.ReplicatedStorage)
local clash = node("Clash", services.Workspace)
local staged = node("Staged", nil)
local strong = node("Strong", services.ReplicatedStorage)

local livePaths = {
	["ReplicatedStorage/A"] = squatter,
	["ReplicatedStorage/Renamed"] = renamed,
	["Workspace/Clash"] = clash,
}
-- The files gave ReplicatedStorage.Renamed the id `a`; Workspace.Clash has
-- the same id in its own store.
local matched = { ReplicatedStorage = { a = renamed }, Workspace = { a = clash } }
local lookups = {
	ReplicatedStorage = { ["debug:7"] = strong, ["2"] = squatter },
	Workspace = { w1 = clash, ["debug:9"] = clash },
}

local environment = {
	game = {
		GetService = function(_, name)
			return services[name]
		end,
	},
	resolvePathSegments = function(segments)
		return livePaths[table.concat(segments, "/")]
	end,
	matchedSettingsInstance = function(serviceName, settingsId)
		return matched[serviceName] and matched[serviceName][settingsId]
	end,
	resolveInstanceBySettingsId = function(serviceName, settingsId)
		return lookups[serviceName] and lookups[serviceName][settingsId]
	end,
	strongSettingsId = function(settingsId)
		return type(settingsId) == "string" and string.sub(settingsId, 1, 6) == "debug:"
	end,
}
local start = assert(source:find("local function decodeRefValue(", 1, true))
local finish = assert(source:find("local function decodeValue(", start, true))
local decodeRefValue = luau.load(source:sub(start, finish - 1) .. "\nreturn decodeRefValue", { environment = environment })()

local ctx = { allowedServices = { ReplicatedStorage = true, Workspace = true } }
local function ref(settingsId, segments)
	return { _type = "Ref", settingsId = settingsId, pathSegments = segments }
end
local function equal(actual, expected, label)
	if actual ~= expected then
		error(string.format("%s: expected %s, got %s", label,
			if expected then expected.Name else "nil", if actual then actual.Name else "nil"))
	end
end

-- The id of the path's service names the target; another instance that now
-- sits at the stored path does not take the reference.
equal(decodeRefValue(ref("a", { "ReplicatedStorage", "A" }), ctx, "Workspace"), renamed, "stale path, known id")
equal(decodeRefValue(ref("debug:7", { "ReplicatedStorage", "A" }), ctx, "Workspace"), strong, "stale path, debug id")
-- The holder's service has the same id; the path says which store it is of.
equal(decodeRefValue(ref("a", { "ReplicatedStorage", "Renamed" }), ctx, "Workspace"), renamed, "id of the path's service")
equal(decodeRefValue(ref("a", nil), ctx, "Workspace"), clash, "id without a path is the holder's service")
-- An instance the same push creates at the path is the target.
ctx.resolveStagedPath = function(segments)
	return if table.concat(segments, "/") == "ReplicatedStorage/A" then staged else nil
end
equal(decodeRefValue(ref("a", { "ReplicatedStorage", "A" }), ctx, "Workspace"), staged, "staged path")
ctx.resolveStagedPath = nil
-- An id the service does not know leaves the path, and a positional id never
-- precedes it.
equal(decodeRefValue(ref("zz", { "ReplicatedStorage", "A" }), ctx, "Workspace"), squatter, "unknown id")
equal(decodeRefValue(ref("2", { "ReplicatedStorage", "Renamed" }), ctx, "Workspace"), renamed, "positional id")
equal(decodeRefValue(ref("zz", { "ReplicatedStorage", "Gone" }), ctx, "Workspace"), nil, "nothing")
-- An id only another service has is not the target of a path into this one,
-- unless it is an id that is unique in the place. Without a path any service
-- that has the id may hold the target.
equal(decodeRefValue(ref("w1", { "ReplicatedStorage", "Gone" }), ctx, "Workspace"), nil, "id of another service")
equal(decodeRefValue(ref("debug:9", { "ReplicatedStorage", "Gone" }), ctx, "Workspace"), clash, "moved debug id")
equal(decodeRefValue(ref("w1", nil), ctx, "ReplicatedStorage"), clash, "id alone")

print("Reference resolution: the id of the path's service precedes the stored path; a staged path and a place-wide id keep their roles")
