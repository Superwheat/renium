# roblox-sync-rs

Native Rust importer for Roblox MCP snapshots.

## Build

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
cargo build --release
```

Binary output:

- `target/release/roblox-sync-rs.exe`

## Usage

```powershell
roblox-sync-rs.exe import-snapshots --snapshot-dir snapshots --project-root . --services Workspace,ReplicatedStorage --compact-meta-json
```

If `--services` is omitted, all default Roblox services are imported.
