--!nocheck

if not plugin then
	error("Renium must run as a Studio plugin")
end

local runtimeModule = script:FindFirstChild("BridgePluginRuntime")
if not runtimeModule or not runtimeModule:IsA("ModuleScript") then
	error("[Renium] missing child ModuleScript: BridgePluginRuntime")
end

require(runtimeModule).start({
	plugin = plugin,
	rootScript = script,
})
