# Renium (VS Code/Cursor)

This extension controls Renium from VS Code and Cursor through the native CLI.

Renium checks signed updates when the editor opens. **Install Update** installs
matching extension and plugin versions. Reload the editor and restart Studio.
Disable checks with `renium.automaticUpdateChecks`.

## What it does

- Pull, Push, snapshots, and two-way Live Sync
- Git status, pull, commit, and push
- Pull from Studio, commit, and push in one flow
- Wally and reusable link packages
- Optional sync on save
- Status bar menu and output panel

## Commands

- `Renium: Open Menu`
- `Renium: Install Studio Plugin`
- `Renium: Manage Places`
- `Renium: Pull Studio to Files`
- `Renium: Push Files to Studio`
- `Renium: Export Snapshots Only`
- `Renium: Sync Wally Packages`
- `Renium: Start Live Sync`
- `Renium: Stop Live Sync`
- `Renium: Git`

## Multi-place experiences

Use **Manage Places → Add Current Studio Place** for each published place.
Renium checks the `GameId` before writing. Each place has its own project root;
`sourceRoot` changes the default `src` folder:

```text
renium.experience.json
places/
  main/
    renium.project.jsonc
    src/
    sourcemap.json
  lobby/
    renium.project.jsonc
    src/
    sourcemap.json
```

Place files default to `{ "schemaVersion": 1 }`; add only non-default options.

Aliases come from published names: lowercase, spaces to underscores, ASCII
letters and numbers only. **Rename Active Place** moves the folder without
renaming the Roblox place.

**Switch Active Place** selects the target for sync and project commands.
**Reorder Places** changes display order. The workspace stores the active place;
`renium.experience.json` stores order. Without it, the project stays single-place.

## .renium viewer

Drag a `.renium` file onto **Inspector**, or double-click it, to view instances,
properties, attributes, source, and IDs. The CLI uses the same decoder as sync.

## Requirements

- Renium Studio plugin running in Studio
- Renium CLI bundled with the extension, installed on `PATH`, or selected with `renium.cliPath`
- For Wally package sync: `wally` on PATH, or configure `renium.wallySync.wallyPath`
- `git` available on PATH, or configure `renium.gitSync.gitPath`

Releases include the matching CLI and expose `renium` and `rbx` to new terminals.
Projects need no executable copies.

On macOS, installation creates `~/Applications/Renium Studio.app` for
protected-property sync. The original Studio app is unchanged.

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

Run **Sync Wally Packages** to install, import, and optionally apply packages to
Studio. The default target is `<sourceRoot>/ReplicatedStorage/Packages`.

If `wally.toml` is missing, Renium can create it. Set
`renium.wallySync.wallyPath` for custom locations.

## Git tab behavior

The **Git** tab is beside Explorer and History.

- Shows branch, remote, ahead/behind, and changed project files
- Redacts credentials from remote URLs
- Scopes Git operations to the configured source folder
- Can require a clean worktree before pull
- Pauses mirroring during pull and checkout
- Uses fast-forward-only pull
- Rejects unexpected pre-staged files
- Excludes untracked files by default
- Can apply pulled changes to Studio

## Development

```powershell
cd tools/renium-vscode-extension
npm.cmd ci
npm.cmd run verify
```

Normal builds use checked-in API metadata and icons without starting Studio.
Refresh assets explicitly:

```powershell
npm.cmd run sync-assets             # local metadata/icons; no Studio process
npm.cmd run refresh-studio-assets   # also runs Studio's headless -API export
```

Review generated diffs. Release builds don't depend on the installed Studio.

Build the Rust backend:

```powershell
$env:PATH = "$env:USERPROFILE\\.cargo\\bin;$env:PATH"
cd tools/renium
cargo build --locked --release
```

Press `F5` here to run an Extension Development Host.

## Packaging and release builds

Build the VSIX:

```powershell
cd tools/renium-vscode-extension
npm.cmd ci
npm.cmd run package
```

Build all release artifacts from the repository root:

```powershell
.\tools\build-release.ps1 -LocalBuild
```

This writes versioned artifacts, hashes, and a manifest under `dist/`.
`recompile.bat` is the shortcut.

For a public release, omit `-LocalBuild`. It requires a clean checkout, license,
and registered VS Code publisher. `publisher: "local"` supports only private
VSIX installation.

## Key settings

- `renium.cliPath` (optional CLI override; blank uses the bundled CLI)
- `renium.projectRoot` (default: `${workspaceFolder}`)
- `renium.autoSyncOnSave` (default: `false`)
- `renium.autoSyncDebounceMs` (default: `800`)
- `renium.editorLiveSyncEnabled` (default: `false`)
- `renium.studioLiveSyncEnabled` (default: `true`)
- `renium.studioLiveSyncPollMs` (default: `250`, minimum: `10`; backs off while idle or after errors)
- `renium.progressHeartbeatSeconds` (default: `2`)
- `renium.gitSync.gitPath` (default: `git`)
- `renium.gitSync.remote` (default: `origin`)
- `renium.gitSync.branch` (blank = current branch)
- `renium.gitSync.autoFetch` (default: `true`)
- `renium.gitSync.pullFromStudioBeforePush` (`ask`, `always`, `never`)
- `renium.gitSync.stageMode` (`tracked` or `configuredPaths`)
- `renium.gitSync.stagePaths` (defaults to `sourceRoot`; path list used with `configuredPaths`)
- `renium.gitSync.includeUntracked` (default: `false`)
- `renium.gitSync.commitMessageTemplate` (supports `${date}`, `${datetime}`, `${branch}`)
- `renium.gitSync.confirmBeforePush` (default: `true`)
- `renium.gitSync.requireCleanWorktreeBeforePull` (default: `true`)
- `renium.gitSync.applyPulledChangesToStudio` (`ask`, `always`, `never`)
- `renium.gitSync.timeoutSeconds` (default: `120`)
- `renium.gitSync.outputBehavior` (`onStart`, `onError`, `silent`)
- `renium.wallySync.wallyPath` (default: `wally`)
- `renium.wallySync.packagesDir` (default: `Packages`)
- `renium.wallySync.targetService` (default: `ReplicatedStorage`)
- `renium.wallySync.targetName` (default: `Packages`)
- `renium.wallySync.serverPackagesDir` (default: `ServerPackages`)
- `renium.wallySync.serverTargetService` (default: `ServerStorage`)
- `renium.wallySync.serverTargetName` (default: `ServerPackages`)
- `renium.wallySync.devPackagesDir` (default: `DevPackages`)
- `renium.wallySync.devTargetService` (default: `ReplicatedStorage`)
- `renium.wallySync.devTargetName` (default: `DevPackages`)
- `renium.wallySync.runInstall` (default: `true`)
- `renium.wallySync.applyToStudio` (`ask`, `always`, `never`)
