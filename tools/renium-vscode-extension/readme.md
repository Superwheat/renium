# Renium for VS Code and Cursor

Edit scripts and instances in your editor while Studio and project files stay in sync.
[Installation and CLI guide](../renium/README.md).

## Start

1. Open the intended place in Studio and a dedicated project folder in your editor.
2. For a new project, run **Renium: Pull Studio to Files**.
3. Enable **Renium: Start Live Sync**. For an existing project, start Live Sync
   without first pulling over your files.

Saves flow to Studio; Studio edits flow to files. File edits during Play wait
for Edit mode. Healthy Live Sync needs no push or playtest after each save.
Conflicts ask which version to keep.

Use **Renium: Open Menu** for sync, places, packages, Git, settings, and diagnostics.
The Output panel reports errors and pending work.

## Explorer and Inspector

Browse the project hierarchy, search instances, and edit properties/attributes
without opening Studio. Drag a `.renium` store onto **Inspector**, or double-click
it, to inspect instances, source, references, and IDs.

## Multiple places

Use **Manage Places → Add Current Studio Place** for each published place.
Renium checks the GameId and creates separate project folders:

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

**Switch Active Place** chooses the sync target. **Rename Active Place** moves
its folder, not its Roblox name. **Reorder Places** changes display order.

Aliases derive from published names. Place configs default to
`{ "schemaVersion": 1 }`; `sourceRoot` overrides `src`.
The workspace remembers the active place; the experience file stores order.
Without an experience file, the project stays single-place.

## Packages and Git

**Sync Wally Packages** installs/imports dependencies from `wally.toml`.
Renium can create a missing manifest. Shared packages default to
`<sourceRoot>/ReplicatedStorage/Packages`; server/dev targets are configurable.
Applying packages to Studio is a separate choice.

The **Git** tab shows branch, remote, ahead/behind, and project changes.
It uses fast-forward-only pull, pauses mirroring during pull/checkout, rejects
unexpected staged files, and excludes untracked files by default. Credentials
are redacted from remote URLs. Applying pulled files to Studio is optional.
Git push does not publish a Roblox place or package.

## Requirements and settings

The extension bundles the matching CLI and exposes `rbx`/`renium` in new terminals.
No executable copies belong in projects. Studio operations need the Renium plugin;
Git and Wally workflows need their project-configured tools.

Search **Renium** in editor Settings for descriptions, defaults, and allowed values.
Common groups:

| Setting | Purpose |
|---|---|
| `renium.cliPath` | Optional CLI override; blank uses the bundle |
| `renium.projectRoot` | Project folder; defaults to the workspace |
| `renium.autoSyncOnSave` / `autoSyncDebounceMs` | Manual-sync save behavior |
| `renium.editorLiveSyncEnabled` / `studioLiveSyncEnabled` | Sync directions |
| `renium.studioLiveSyncPollMs` | Studio polling interval |
| `renium.gitSync.*` | Git tool, scope, staging, confirmations, and Studio application |
| `renium.wallySync.*` | Wally tool, package folders, realm targets, and install behavior |

On macOS, use installed `~/Applications/Renium Studio.app` for protected-property
sync; the original Studio app stays unchanged.

Signed update checks run when the editor opens. **Install Update** installs
matching components; reload the editor and restart Studio afterward.
`renium.automaticUpdateChecks` controls these checks.

## Development

From this extension folder:

```powershell
npm.cmd ci
npm.cmd run verify
npm.cmd run package
```

Use `npm` on macOS/Linux. Press F5 in VS Code for an Extension Development Host.
Edit `src/`, not generated `out/`.

Build the backend from the repository root:

```powershell
cargo build --locked --release --manifest-path tools/renium/Cargo.toml
./tools/build-release.ps1 -LocalBuild
```

The bundle goes under `dist/`; `recompile.bat` is a shortcut.
Public releases omit `-LocalBuild` and require a clean checkout, license,
and registered publisher; publisher `local` is for private VSIX installs.

Builds use checked-in metadata/icons, not a running Studio.
`npm run sync-assets` syncs local assets; `npm run refresh-studio-assets`
also invokes Studio's headless API export. Review generated diffs.
