# Roblox MCP Sync (VS Code/Cursor)

This extension gives Rojo/Argon-style control from VS Code/Cursor using native Rust executables:

- `tools/roblox-sync-rs/target/release/roblox-sync-rs.exe` for Studio export/import/serialization

## What it does

- Full sync command: Studio -> snapshots -> `src` + `default.project.generated.json`
- Export-only command: Studio -> snapshots
- Import-only command: snapshots -> `src` (Rust importer)
- Live sync command is intentionally disabled in the rust-only build
- Optional debounced auto-sync on save
- Status bar button + quick menu + output panel

## Commands

- `Roblox Sync: Open Menu`
- `Roblox Sync: Full Sync (Studio -> src)`
- `Roblox Sync: Export Snapshots Only`
- `Roblox Sync: Import Snapshots Into src`
- `Roblox Sync: Start Live Sync (Editor -> Studio)`
- `Roblox Sync: Stop Live Sync`
- `Roblox Sync: Sync Active Service Now`

## Requirements

- Roblox MCP bridge/plugin running in Studio
- Native executables:
  - `tools/roblox-sync-rs/target/release/roblox-sync-rs.exe`

## Development

```powershell
cd tools/vscode-mcp-sync-extension
npm.cmd install
npm.cmd run compile
```

Build Rust backend (recommended):

```powershell
$env:PATH = "$env:USERPROFILE\\.cargo\\bin;$env:PATH"
cd tools/roblox-sync-rs
cargo build --release
```

Press `F5` in VS Code from this extension folder to run an Extension Development Host.

## Packaging

```powershell
cd tools/vscode-mcp-sync-extension
npx.cmd @vscode/vsce package
```

Then install the produced `.vsix` in VS Code/Cursor.

## Key settings

- `robloxSync.exportCliPath`
- `robloxSync.editorSyncCliPath`
- `robloxSync.rustCliPath` (path to `roblox-sync-rs.exe`)
- `robloxSync.projectRoot` (default: `${workspaceFolder}`)
- `robloxSync.transport` (`ws` or `mcp`)
- `robloxSync.runImport` (default: `true`)
- `robloxSync.autoSyncOnSave` (default: `false`)
- `robloxSync.autoSyncDebounceMs` (default: `800`)
- `robloxSync.progressHeartbeatSeconds` (default: `6`)
