use mlua::Lua;

#[test]
fn device_simulator_deadline_releases_later_calls() -> mlua::Result<()> {
    let lua = Lua::new();
    lua.load(include_str!(
        "../../plugin_ws_bridge/tests/device-simulator-workloads.luau"
    ))
    .exec()?;
    let api: mlua::Table = lua
        .load(include_str!(
            "../../plugin_ws_bridge/BridgeRuntimeApi.module.luau"
        ))
        .eval()?;
    lua.globals().set("RuntimeApi", api)?;
    lua.load("runDeviceSimulatorWorkloads(RuntimeApi)").exec()
}
