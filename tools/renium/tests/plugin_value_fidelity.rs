use mlua::Lua;

#[test]
fn plugin_preserves_small_edits_and_rejects_missing_writes() -> mlua::Result<()> {
    let lua = Lua::new();
    // Only the engine value representation is stubbed; comparisons and the
    // mutation/verification decisions below are the production plugin code.
    lua.load(r#"
        local builtinTypeof = typeof
        typeof = function(value)
            return if type(value) == "table" and value.engineType then value.engineType else builtinTypeof(value)
        end
    "#).exec()?;
    let equality: mlua::Table = lua
        .load(include_str!(
            "../../plugin_ws_bridge/BridgeValueEquality.module.lua"
        ))
        .eval()?;
    lua.globals().set("equality", equality)?;
    lua.load(r#"
        valuesEqual = equality.valuesEqual
        exactValuesEqual = equality.exactValuesEqual
        assert(not valuesEqual(1, 1.00001), "small numeric differences must not verify")
        local function color(r) return { engineType = "Color3", R = r, G = 0.5, B = 0.5 } end
        assert(not valuesEqual(color(0.5), color(0.5019)), "sub-byte Color3 differences must not verify")
        assert(valuesEqual(color(0.5), color(0.5 + 1e-8)), "f32 readback rounding must be accepted")
        assert(valuesEqual(0/0, 0/0))
        assert(not valuesEqual(math.huge, 1))
        assert(not valuesEqual(math.huge, -math.huge))
        local function frame(values)
            return { engineType = "CFrame", GetComponents = function() return table.unpack(values) end }
        end
        local a = {0,0,0,1,0/0,0,0,1,0,0,0,1}
        local b = table.clone(a)
        assert(valuesEqual(frame(a), frame(b)), "nonfinite CFrame reflexivity")
        b[1] = 0.00001
        assert(not valuesEqual(frame(a), frame(b)), "position precision")
        readProperty = function(instance, name) return true, instance[name] end
        writePropertyForSync = function(instance, name, value) instance[name] = value return true end
        decodeValue = function(value) return true, value end
        setAttributeForSync = function(instance, name, value) instance[name] = value return true end
        isEngineManagedAttribute = function(name) return string.sub(tostring(name), 1, 4) == "RBX_" end
    "#).exec()?;
    let source = include_str!("../../plugin_ws_bridge/BridgeEditorSync.module.lua");
    let body = source
        .split("local function propertyValuesEqual(")
        .nth(1)
        .unwrap()
        .split("BridgeEditorSync.decodeValue")
        .next()
        .unwrap();
    lua.load(format!("function propertyValuesEqual({body}"))
        .exec()?;
    lua.load(r#"
        RbxDomModule = {findCanonicalPropertyDescriptor = function(class, name) return {dataType=name} end}
        assert(not propertyValuesEqual({}, "Float64", 16, 16.00000001))
        assert(propertyValuesEqual({}, "Float32", 0.3, 0.30000001192092896))
        assert(not propertyValuesEqual({}, "Float32", 1, 1.00001))
    "#).exec()?;
    for (name, next) in [
        ("writeDecodedProperty", "isMeshGeometryProperty"),
        ("applyChangedAttribute", "recordVerifyMismatch"),
    ] {
        let body = source
            .split(&format!("local function {name}("))
            .nth(1)
            .unwrap()
            .split(&format!("local function {next}("))
            .next()
            .unwrap();
        lua.load(format!("function {name}({body}")).exec()?;
    }
    lua.load(
        r#"
        local instance = { Value = 1, GetAttribute = function(self, name) return self[name] end }
        local stats = {noops=0, propertyUpdated=0, attributeUpdated=0}
        assert(writeDecodedProperty(instance, "Value", 1.00001, {}, stats, false))
        assert(instance.Value == 1.00001 and stats.propertyUpdated == 1 and stats.noops == 0)
        instance.Value = 16
        applyChangedAttribute(instance, "Value", 16.00000001, {}, {}, stats)
        assert(instance.Value == 16.00000001 and stats.attributeUpdated == 1 and stats.noops == 0,
            "double-precision attributes must not use f32 tolerance to skip writes")
    "#,
    )
    .exec()
}
