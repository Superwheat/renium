# Renium (VS Code/Cursor)

This extension gives Rojo/Argon-style control from VS Code/Cursor using native Rust executables:

- `renium.exe` for Studio export/import/serialization

## What it does

- Full sync command: Studio -> snapshots -> `src` + `default.project.generated.json`
- Export-only command: Studio -> snapshots
- Import-only command: snapshots -> `src` (Rust importer)
- Two-way live sync between `src` and Studio, including dirty Studio service imports
- Git tab inside the main Renium panel with repository status plus pull/commit/push actions
- Optional "Full Sync, then commit & push" flow so Studio/export changes can be published in one workflow
- Wally package sync that runs `wally install` and imports packages directly into the configured package target
- Reusable link packages; treat third-party `.renium` packages as source code because they can contain Luau scripts, auto-running script classes, properties, and PackageLink instances
- Optional debounced auto-sync on save
- Status bar button + quick menu + output panel

## Commands

- `Renium: Open Menu`
- `Renium: Full Sync (Studio -> src)`
- `Renium: Export Snapshots Only`
- `Renium: Sync Wally Packages`
- `Renium: Start Live Sync (Editor -> Studio)`
- `Renium: Stop Live Sync`
- `Renium: Git`

## .renium viewer

Open the Renium panel and switch to the **Inspector** tab (next to Explorer /
History / Git), then drag any `.rbsync` file onto it to see its instance tree —
class icons, properties, attributes, script source, and settings id, with a
filter box. You can also double-click a `.renium` in the file Explorer to open
the same view full-width. Decoding is done by the `renium` CLI, so what you see
matches exactly what syncs. Legacy `.rbsync` stores remain readable.

## Requirements

- Roblox MCP bridge/plugin running in Studio
- Native executables:
  - `renium.exe` in the workspace root or `bin/renium.exe`; source-repository build folders are also detected
- For Wally package sync: `wally` on PATH, or configure `renium.wallySync.wallyPath`
- `git` available on PATH, or configure `renium.gitSync.gitPath`

## Wally package sync

Use Wally normally from the project root:

```toml
[package]
name = "local/my-game"
version = "0.1.0"
registry = "https://github.com/UpliftGames/wally-index"
realm = "shared"

[dependencies]
```

Then run `Renium: Sync Wally Packages` from the command palette or Renium menu. Renium runs `wally install`, imports the generated package tree directly, replaces the configured package target, and can apply the package tree to Studio. By default, that target is `src/ReplicatedStorage/Packages`.

If `wally.toml` is missing, the VS Code command can create a starter manifest. If you use Aftman shims or a custom tool location, set `renium.wallySync.wallyPath`.

## Git tab behavior

The **Git** tab lives inside the main Renium panel alongside the existing Explorer and History tabs.

- Shows current branch, remote, ahead/behind counts, and changed `src/` files
- Redacts credentials/tokens before remote URLs are shown in the UI or output
- Uses `src/` as the default and canonical Git sync scope for staging/status
- Blocks pull when the worktree is dirty if `renium.gitSync.requireCleanWorktreeBeforePull` is enabled
- Stops live sync before pull so branch updates do not race with editor/Studio mirroring
- Uses fast-forward-only pull to avoid creating merge commits silently
- Blocks commit/push when files are already staged, to avoid publishing unintended index state
- Excludes untracked files by default unless `renium.gitSync.includeUntracked` is enabled
- Can optionally push pulled `src` changes back into Studio after a successful pull

## Development

```powershell
cd tools/renium-vscode-extension
npm.cmd ci
npm.cmd run verify
```

Ordinary compile, verify, package, and release commands use the checked-in API
metadata and class icons. They do not start Roblox Studio or rewrite generated
source files. Asset refreshes are explicit:

```powershell
npm.cmd run sync-assets             # local metadata/icons; no Studio process
npm.cmd run refresh-studio-assets   # also runs Studio's headless -API export
```

Review and commit the generated diff after either refresh. Release builds must
not depend on whichever Studio version happens to be installed on the build
machine.

Build Rust backend (recommended):

```powershell
$env:PATH = "$env:USERPROFILE\\.cargo\\bin;$env:PATH"
cd tools/renium
cargo build --locked --release
```

Press `F5` in VS Code from this extension folder to run an Extension Development Host.

## Packaging and release builds

Build just the extension VSIX from its source directory:

```powershell
cd tools/renium-vscode-extension
npm.cmd ci
npm.cmd run package
```

For the CLI, extension, and both Studio plugin formats together, run this from
the repository root:

```powershell
.\tools\build-release.ps1 -LocalBuild
```

It writes a versioned directory under `dist/`, regenerates both plugin bundles
from `Renium.project.json`, validates the VSIX metadata, and writes hashes plus
a build manifest. `recompile.bat` is the same local-build shortcut.

For a public release, omit `-LocalBuild`. The release command intentionally
requires a clean checkout, a root product `LICENSE` file, and a registered VS
Code publisher. The current `publisher: "local"` setting is suitable only for
offline/private VSIX installation; replace it with your registered publisher
before Marketplace publication.

## Key settings

- `renium.exportCliPath`
- `renium.editorSyncCliPath`
- `renium.rustCliPath` (path to `renium.exe`)
- `renium.projectRoot` (default: `${workspaceFolder}`)
- `renium.transport` (`ws` or `mcp`)
- `renium.runImport` (default: `true`)
- `renium.autoSyncOnSave` (default: `false`)
- `renium.autoSyncDebounceMs` (default: `800`)
- `renium.editorLiveSyncEnabled` (default: `false`)
- `renium.editorLiveSyncOnStartup` (default: `false`, legacy)
- `renium.studioLiveSyncEnabled` (default: `true`)
- `renium.studioLiveSyncPollMs` (default: `250`, minimum: `10`; backs off while idle or after errors)
- `renium.progressHeartbeatSeconds` (default: `2`)
- `renium.usePersistentBridge` (default: `true`)
- `renium.gitSync.gitPath` (default: `git`)
- `renium.gitSync.remote` (default: `origin`)
- `renium.gitSync.branch` (blank = current branch)
- `renium.gitSync.autoFetch` (default: `true`)
- `renium.gitSync.runFullSyncBeforePush` (`ask`, `always`, `never`)
- `renium.gitSync.stageMode` (`tracked` or `configuredPaths`)
- `renium.gitSync.stagePaths` (defaults to `src`; path list used with `configuredPaths`)
- `renium.gitSync.includeUntracked` (default: `false`)
- `renium.gitSync.commitMessageTemplate` (supports `${date}`, `${datetime}`, `${branch}`)
- `renium.gitSync.confirmBeforePush` (default: `true`)
- `renium.gitSync.requireCleanWorktreeBeforePull` (default: `true`)
- `renium.gitSync.applyPulledChangesToStudio` (`ask`, `always`, `never`)
- `renium.gitSync.timeoutSeconds` (default: `120`)
- `renium.gitSync.outputBehavior` (`onStart`, `onError`, `silent`)
- `renium.wallySync.wallyPath` (default: `wally`)
- `renium.wallySync.rojoPath` (deprecated; ignored)
- `renium.wallySync.packagesDir` (default: `Packages`)
- `renium.wallySync.targetService` (default: `ReplicatedStorage`)
- `renium.wallySync.targetName` (default: `Packages`)
- `renium.wallySync.runInstall` (default: `true`)
- `renium.wallySync.applyToStudio` (`ask`, `always`, `never`)
