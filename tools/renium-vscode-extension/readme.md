# Renium (VS Code/Cursor)

This extension gives Rojo/Argon-style control from VS Code/Cursor using native Rust executables:

- `renium.exe` for Studio export/import/serialization

## What it does

- Separate Pull Studio to Files and Push Files to Studio commands
- Export-only command: Studio -> snapshots
- Import-only command: snapshots -> the configured project source folder (Rust importer)
- Two-way live sync between project files and Studio, including dirty Studio service imports
- Git tab inside the main Renium panel with repository status plus pull/commit/push actions
- Optional "Pull from Studio, Commit and Push" flow so Studio changes can be published in one workflow
- Wally package sync that runs `wally install` and imports packages directly into the configured package target
- Reusable link packages; treat third-party `.renium` packages as source code because they can contain Luau scripts, auto-running script classes, properties, and PackageLink instances
- Optional debounced auto-sync on save
- Status bar button + quick menu + output panel

## Commands

- `Renium: Open Menu`
- `Renium: Install Studio Plugin`
- `Renium: Manage Places`
- `Renium: Pull Studio to Files`
- `Renium: Push Files to Studio`
- `Renium: Export Snapshots Only`
- `Renium: Sync Wally Packages`
- `Renium: Start Live Sync (Editor -> Studio)`
- `Renium: Stop Live Sync`
- `Renium: Git`

## Multi-place experiences

Open `Renium: Manage Places`, then choose **Add Current Studio Place** for each
published place in an experience.
Renium verifies that its `GameId` matches the project before creating or
changing files. Each place gets an independent project root. `src` is the
default source folder; `sourceRoot` in `renium.project.jsonc` can change it:

```text
renium.experience.json
places/
  main/
    src/
    sourcemap.json
  lobby/
    src/
    sourcemap.json
```

The first alias comes from the published place name. It is lowercased, spaces
become underscores, and non-ASCII letters and punctuation are removed. Choose
**Rename Active Place** to use a shorter alias such as `main` or `lobby`.
Renaming the alias moves that place folder and does not rename the place on
Roblox.

**Switch Active Place** changes the one place that Pull, Push, live sync,
Explorer, generated files, and package commands use. **Reorder Places**
controls how places are listed. Both are under **Manage Places**. The active
selection is stored per workspace, while the display order is stored in
`renium.experience.json`. Projects without that file keep the existing
single-place layout and behavior.

## .renium viewer

Open the Renium panel and switch to the **Inspector** tab (next to Explorer /
History / Git), then drag any `.renium` file onto it to see its instance tree:
class icons, properties, attributes, script source, and settings id, with a
filter box. You can also double-click a `.renium` in the file Explorer to open
the same view full-width. Decoding is done by the `renium` CLI, so what you see
matches exactly what syncs.

## Requirements

- Renium Studio plugin running in Studio
- Renium CLI bundled with the extension, installed on `PATH`, or selected with `renium.cliPath`
- For Wally package sync: `wally` on PATH, or configure `renium.wallySync.wallyPath`
- `git` available on PATH, or configure `renium.gitSync.gitPath`

The released extension carries its matching Renium CLI and exposes `renium`
and `rbx` to new integrated terminals. Projects do not need copies of
`renium.exe`, `rbx.cmd`, or the extension itself. A CLI already on `PATH` and
the older project-local locations remain supported.

On macOS, **Renium: Install Studio Plugin** also prepares
`~/Applications/Renium Studio.app`. Open that app for exact protected-property
sync without save or export dialogs. The original Roblox Studio app remains
unchanged.

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

Then run `Renium: Sync Wally Packages` from the command palette or Renium menu. Renium runs `wally install`, imports the generated package tree directly, replaces the configured package target, and can apply the package tree to Studio. By default, that target is `<sourceRoot>/ReplicatedStorage/Packages`.

If `wally.toml` is missing, the VS Code command can create a starter manifest. If you use Aftman shims or a custom tool location, set `renium.wallySync.wallyPath`.

## Git tab behavior

The **Git** tab lives inside the main Renium panel alongside the existing Explorer and History tabs.

- Shows current branch, remote, ahead/behind counts, and changed project source files
- Redacts credentials/tokens before remote URLs are shown in the UI or output
- Uses the configured project source folder as the default Git sync scope for staging/status
- Blocks pull when the worktree is dirty if `renium.gitSync.requireCleanWorktreeBeforePull` is enabled
- Stops live sync before pull so branch updates do not race with editor/Studio mirroring
- Uses fast-forward-only pull to avoid creating merge commits silently
- Blocks commit/push when files are already staged, to avoid publishing unintended index state
- Excludes untracked files by default unless `renium.gitSync.includeUntracked` is enabled
- Can optionally push pulled project-file changes back into Studio after a successful pull

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

- `renium.cliPath` (optional CLI override; blank uses the bundled CLI)
- `renium.projectRoot` (default: `${workspaceFolder}`)
- `renium.runImport` (default: `true`)
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
