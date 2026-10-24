//! Runs the same dependency-injected workloads as Lune, without Studio or sockets.
use mlua::{Lua, LuaSerdeExt, Value};

#[test]
fn teleport_plugin_lifecycle_workloads() -> mlua::Result<()> {
    let lua = Lua::new();
    let files = lua.create_table()?;
    files.set(
        "tools/plugin_ws_bridge/BridgePlayTeleport.module.luau",
        include_str!("../../plugin_ws_bridge/BridgePlayTeleport.module.luau"),
    )?;
    files.set(
        "tools/plugin_ws_bridge/BridgeTeleportContent.module.luau",
        include_str!("../../plugin_ws_bridge/BridgeTeleportContent.module.luau"),
    )?;
    files.set(
        "tools/plugin_ws_bridge/BridgeTeleportGui.module.luau",
        include_str!("../../plugin_ws_bridge/BridgeTeleportGui.module.luau"),
    )?;
    files.set(
        "tools/plugin_ws_bridge/TeleportShim.module.luau",
        include_str!("../../plugin_ws_bridge/TeleportShim.module.luau"),
    )?;
    lua.globals().set("fixtureFiles", files)?;
    let state: mlua::Table = lua
        .load(include_str!(
            "../../plugin_ws_bridge/BridgeTeleportState.module.luau"
        ))
        .eval()?;
    lua.globals().set("fixtureState", state)?;
    lua.globals().set(
        "encodeJson",
        lua.create_function(|lua, value: Value| {
            let value: serde_json::Value = lua.from_value(value)?;
            serde_json::to_string(&value).map_err(mlua::Error::external)
        })?,
    )?;
    lua.globals().set(
        "decodeJson",
        lua.create_function(|lua, value: String| {
            let value: serde_json::Value =
                serde_json::from_str(&value).map_err(mlua::Error::external)?;
            lua.to_value(&value)
        })?,
    )?;
    let base64: mlua::Table = lua
        .load(include_str!("../../plugin_ws_bridge/tests/base64.luau"))
        .eval()?;
    lua.globals().set("fixtureBase64", base64)?;
    lua.load(r#"
        local libraries = {
            ["@lune/fs"] = { readFile = function(path) return assert(fixtureFiles[path], path) end },
            ["@lune/luau"] = { load = function(source, options)
                local fn = assert(loadstring(source))
                local environment = options.environment
                setmetatable(environment, {__index = getfenv()})
                setfenv(fn, environment)
                return fn
            end },
            ["@lune/serde"] = {
                encode = function(kind, value) assert(kind == "json"); return encodeJson(value) end,
                decode = function(kind, value) assert(kind == "json"); return decodeJson(value) end,
            },
            ["./base64"] = fixtureBase64,
            ["../BridgeTeleportState.module"] = fixtureState,
        }
        require = function(name) return assert(libraries[name], name) end
    "#).exec()?;
    lua.load(include_str!(
        "../../plugin_ws_bridge/tests/teleport-workloads.luau"
    ))
    .set_name("teleport-workloads")
    .exec()?;
    lua.load(include_str!(
        "../../plugin_ws_bridge/tests/teleport-content-workloads.luau"
    ))
    .set_name("teleport-content-workloads")
    .exec()?;
    lua.load(include_str!(
        "../../plugin_ws_bridge/tests/teleport-transplant-workloads.luau"
    ))
    .set_name("teleport-transplant-workloads")
    .exec()?;
    lua.load(include_str!(
        "../../plugin_ws_bridge/tests/teleport-gui-workloads.luau"
    ))
    .set_name("teleport-gui-workloads")
    .exec()?;
    lua.load(include_str!(
        "../../plugin_ws_bridge/tests/teleport-shim-workloads.luau"
    ))
    .set_name("teleport-shim-workloads")
    .exec()
}
