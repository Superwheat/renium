# Parallel Export Bridge Plugin

`ParallelExportBridge.plugin.lua` is a Studio plugin script that opens multiple WebSocket channels and serves export RPC calls.

## What this gives you

- 4 parallel channels by default (`8781,8782,8783,8784`)
- Shared in-plugin export state and chunked responses
- RPC methods matching the exporter pipeline:
  - `ping`
  - `prepare`
  - `getInstanceBatchChunk`
  - `getClassDefaultsChunk`
  - `getScriptPathsChunk`
  - `getSourceChunk`
  - `release`

## Import in Studio

1. Open Studio.
2. Create/import a plugin script.
3. Paste `ParallelExportBridge.plugin.lua`.
4. Parent these ModuleScripts under that plugin script:
   - `BridgeSettings` from `BridgeSettings.module.lua`
   - `BridgeTheme` from `BridgeTheme.module.lua`
   - `BridgeStatus` from `BridgeStatus.module.lua`
   - `RbxDom` module tree (optional but recommended for full property coverage)
5. Enable/run plugin.
6. Confirm plugin widget shows channels as `OPEN`.
`BridgeSettings`, `BridgeTheme`, and `BridgeStatus` are required; `RbxDom` is optional fallback support.

## Prebuilt bundle (modules parented)

Use [ParallelExportBridge.bundle.rbxmx](E:/Documents/rblx/projest/tools/plugin_ws_bridge/ParallelExportBridge.bundle.rbxmx) to import a Script that already has:

- `BridgeSettings` ModuleScript
- `BridgeTheme` ModuleScript
- `BridgeStatus` ModuleScript
- `RbxDom` module tree (sourced from `tools/plugin_ws_bridge/rbx_dom_lua`)

The bundle is built from [ParallelExportBridge.project.json](E:/Documents/rblx/projest/tools/plugin_ws_bridge/ParallelExportBridge.project.json).

When the `RbxDom` module tree is present, the plugin automatically derives class/property export candidates from the Luau `rbx_dom_lua` database instead of relying on the small hardcoded property list.

## Local daemon (test harness)

`ws_bridge_daemon.py` is a local test daemon that accepts plugin connections.

Install dependency:

```bash
pip install websockets
```

Run:

```bash
python tools/plugin_ws_bridge/ws_bridge_daemon.py --host 127.0.0.1 --ports 8781,8782,8783,8784
```

Important:

- `mcp_sync_export.py --transport ws` starts its own bridge server on the same ports.
- Do not run `ws_bridge_daemon.py` at the same time unless you use different ports for one of them.

Then in daemon console:

- `status`
- `ping`
- `prepare Workspace`
- `batch Workspace 1 600`

## Wire protocol (JSON over WebSocket)

Request:

```json
{
  "id": "req-1",
  "method": "prepare",
  "params": { "service": "Workspace" }
}
```

Success response:

```json
{
  "id": "req-1",
  "ok": true,
  "result": { "service": "Workspace", "instanceCount": 1234 },
  "channel": 1
}
```

Error response:

```json
{
  "id": "req-1",
  "ok": false,
  "error": "State not prepared for service: Workspace",
  "channel": 1
}
```

## Notes

- This is Studio-only.
- Roblox currently limits active WebStream clients; keep channel count conservative.
- Close/reconnect channels if your daemon restarts.
