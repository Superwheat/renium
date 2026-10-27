use mlua::Lua;

#[test]
fn fallback_enumeration_publishes_only_after_its_last_yield() -> mlua::Result<()> {
    let lua = Lua::new();
    lua.load(r#"
        local now, generation = 0, 0
        os.clock = function() return now end
        task = {wait = function() generation += 1 end}
        Config = {LUA_SOURCE_CLASS={Script=true}, studioChanges={serviceGeneration=function() return generation end}}
        ALLOWED_SERVICES = {Workspace=true}
        stateByService = {}
        local service = {Name="Workspace", ClassName="Workspace", Archivable=true}
        local descendants = {}
        for i=1,200 do descendants[i] = {Name=tostring(i), ClassName="Script", Parent=service} end
        service.GetDescendants = function() return table.clone(descendants) end
        game = {GetService=function() return service end}
        excludedExportRoots = function() return {} end
        isNeverSerialized = function() return false end
        refreshMatchedSettingsIds = function() end
        IdentityModule = {getCachedScriptSourceKey=function() now += 0.001 end}
        getServicePropertySchema = function() return {} end
        getServiceEnumValueNamesByType = function() return {} end
    "#).exec()?;
    let parallel: mlua::Table = lua
        .load(include_str!(
            "../../plugin_ws_bridge/BridgeParallel.module.luau"
        ))
        .eval()?;
    lua.globals().set("ParallelModule", parallel)?;
    let source = include_str!("../../plugin_ws_bridge/BridgePluginRuntime.module.luau");
    let body = source
        .split("prepareService = function(")
        .nth(1)
        .unwrap()
        .split("function Config.getBridgeInfo(")
        .next()
        .unwrap();
    lua.load(format!("prepareService = function({body}"))
        .exec()?;
    lua.load(r#"
        local ok, message = pcall(prepareService, "Workspace")
        assert(not ok and string.find(tostring(message), "Studio changed Workspace", 1, true))
        assert(stateByService.Workspace == nil, "late script indexing must not publish a mixed export")
    "#).exec()
}

#[test]
fn bulk_instance_reconcile_checks_ownership_after_yielding() -> mlua::Result<()> {
    let lua = Lua::new();
    lua.load(r#"
        now, yields, writes, owned = 0, 0, 0, true
        os.clock = function() return now end
        task = {wait = function() yields += 1; owned = false end}
        local entries = {}
        for i=1,200 do entries[i] = {previousPathSegments={}, key=tostring(i), className="Part", pathSegments={"Workspace", tostring(i)}} end
        sortedInstanceEntries = function() return entries end
        validatedChangeService = function() return "Workspace", {Name="Workspace"} end
        liveHydrateEnabled = function() return true end
        resolveEntryParent = function() return "parent" end
        syncDesiredEntry = function(entry, _, _, stats)
            assert(owned, "must never mutate after losing ownership")
            writes += 1
            now += 0.001
            stats.instanceCreated += 1
            return {ClassName="Part", Name=entry.key, Parent="parent"}
        end
    "#).exec()?;
    let parallel: mlua::Table = lua
        .load(include_str!(
            "../../plugin_ws_bridge/BridgeParallel.module.luau"
        ))
        .eval()?;
    lua.globals().set("ParallelModule", parallel)?;
    let source = include_str!("../../plugin_ws_bridge/BridgeEditorSync.module.luau");
    let body = source
        .split("local function applyInstanceUpserts(")
        .nth(1)
        .unwrap()
        .split("local function applyInstanceDeletes(")
        .next()
        .unwrap();
    lua.load(format!("function applyInstanceUpserts({body}"))
        .exec()?;
    lua.load(r#"
        local budget = ParallelModule.newWorkBudget()
        local ctx = {yieldForBudget = function()
            if budget() then assert(owned, "ownership lost after yielding") end
        end}
        local stats = {instanceCreated=0, instanceReplaced=0, propertyUpdated=0}
        local ok, message = pcall(applyInstanceUpserts, {}, ctx, stats, {})
        assert(not ok and string.find(tostring(message), "ownership lost after yielding", 1, true))
        assert(yields == 1 and writes >= 8 and writes <= 9, "a single bulk change must yield and stop at cancellation")
    "#).exec()
}

#[test]
fn fallback_serialization_yields_and_rejects_interleaved_edits() -> mlua::Result<()> {
    let lua = Lua::new();
    lua.load(r#"
        now, yields, generation = 0, 0, 0
        os.clock = function() return now end
        task = {wait = function()
            yields += 1
            if editOnYield then generation += 1 end
        end}
        Config = {
            studioChanges = {serviceGeneration = function() return generation end},
            getCompactInstanceBatchVariantCacheKey = function() return "batch" end,
            boundedPositiveInteger = function(value, default, max) return math.min(value or default, max) end,
            getHotPropertySchema = function() return {count=1} end,
            acquireDemandSerializerSlot = function() acquired = true end,
            releaseDemandSerializerSlot = function() acquired = false end,
            exportCompactV5InstanceIndexed = function(_, _, index)
                now += 0.001
                return index
            end,
            cacheBatchPayload = function(cache, _, key, value) cache[key] = value end,
        }
        state = {instances={}, classNameByIndex={}, classNames={"Part"}, batchCacheByKey={}, batchCacheKeys={}}
        for i=1,200 do state.instances[i]={ClassName="Part"} end
        getState = function() return state end
        refreshMatchedSettingsIds = function() return false end
        ChunkingModule = {jsonEncodeTimed = function(payload) return payload, 0 end}
    "#).exec()?;
    let parallel: mlua::Table = lua
        .load(include_str!(
            "../../plugin_ws_bridge/BridgeParallel.module.luau"
        ))
        .eval()?;
    lua.globals().set("ParallelModule", parallel)?;
    let source = include_str!("../../plugin_ws_bridge/BridgePluginRuntime.module.luau");
    let body = source
        .split("function Config.getInstanceBatchCompact(")
        .nth(1)
        .unwrap()
        .split("function Config.cacheBatchPayload(")
        .next()
        .unwrap();
    lua.load(format!("function Config.getInstanceBatchCompact({body}"))
        .exec()?;
    lua.load(r#"
        local small = Config.getInstanceBatchCompact("Workspace", 1, 2)
        assert(#small.items == 2 and yields == 0, "small work stays synchronous")
        table.clear(state.batchCacheByKey)
        local batch = Config.getInstanceBatchCompact("Workspace", 1, 200)
        assert(yields >= 20 and yields <= 26, "large exports must give Studio a frame every 8ms")
        for index, item in batch.items do assert(item == index, "yielding preserves row order and contents") end
        assert(not acquired)
        table.clear(state.batchCacheByKey)
        editOnYield = true
        local ok, message = pcall(Config.getInstanceBatchCompact, "Workspace", 1, 200)
        assert(not ok and string.find(tostring(message), "Studio changed Workspace", 1, true))
        assert(not acquired and next(state.batchCacheByKey) == nil, "mixed exports must never be cached")
    "#).exec()
}
