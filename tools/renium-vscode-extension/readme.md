# Renium for VS Code and Cursor

This extension keeps Roblox Studio and your project files in sync while you edit
scripts and instances in your editor. It bundles the matching CLI and puts `rbx`
and `renium` on PATH in new terminals. The [main README](../../README.md) covers
installing, and the [CLI guide](../renium/README.md) covers the CLI.

## Start

Open the place in Studio and a folder for the project in your editor. For a new
project, run **Renium: Pull from Studio**, then **Renium: Start Live Sync**. For a
project that already has files, start Live Sync without pulling, so your files are
not replaced. Edits made during Play wait for Edit mode, and a conflict asks which
version to keep. **Renium: Open Menu** has sync, places, packages, Git, settings
and diagnostics.

## Explorer and Inspector

The Explorer shows the project hierarchy and lets you search instances and edit
properties and attributes without opening Studio. Drag a `.renium` store onto the
Inspector, or double-click it, to see its instances, sources, references and IDs.

## Places, packages and Git

**Renium: Manage Places** adds the connected Studio place to the experience as its
own project folder, and switches, renames or reorders places.
**Renium: Sync Wally Packages** installs the dependencies from `wally.toml`. The
Git tab shows the branch and project changes and pulls fast-forward only. A Git
push never publishes anything to Roblox.

## Settings and updates

Search **Renium** in the editor's Settings to see every option. The extension
checks for signed updates when the editor opens, unless
`renium.automaticUpdateChecks` is off; after an update, reload the editor and
restart Studio. On macOS, protected properties and Terrain need the Studio patch
described in the [main README](../../README.md#the-native-helper).

Questions go to the [Discord server](https://discord.gg/wwTFHSSNn3). Report bugs as
[GitHub issues](https://github.com/Superwheat/renium/issues) with the Renium
version, your OS, the action and the exact error.

## Development

From this folder (use `npm` instead of `npm.cmd` on macOS and Linux):

```powershell
npm.cmd ci
npm.cmd run verify
npm.cmd run package
```

Press F5 in VS Code for an Extension Development Host, and edit `src/`, not the
generated `out/`. [Build from source](../renium/README.md#build-from-source) in the
CLI guide bundles the CLI, extension and plugin under `dist/`. Public releases omit
`-LocalBuild` and need a clean checkout, the license and a registered publisher.
Builds use checked-in Studio metadata and icons; `npm run refresh-studio-assets`
refreshes them from Studio's headless API export, so review the diff it produces.
