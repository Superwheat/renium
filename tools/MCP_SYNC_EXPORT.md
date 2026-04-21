# MCP Auto Export (Rust)

Use [roblox-sync-rs.exe](E:/Documents/rblx/projest/tools/roblox-sync-rs/target/release/roblox-sync-rs.exe) to export these services from the Studio bridge:

- Workspace, Players, Lighting, MaterialService
- ReplicatedFirst, ReplicatedStorage
- ServerScriptService, ServerStorage
- StarterGui, StarterPack, StarterPlayer

It fetches snapshot JSON in chunks and fetches every script `Source` in chunks, then writes:

- `snapshots/<Service>.json`

Run:

```powershell
cd E:\Documents\rblx\projest
.\tools\roblox-sync-rs\target\release\roblox-sync-rs.exe export-snapshots --project-root . --snapshot-dir snapshots --no-run-import
```

If you see a "not connected to Studio" error, open Studio and connect the MCP bridge/plugin first.
