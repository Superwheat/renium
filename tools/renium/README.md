# Renium

Renium keeps Roblox Studio and project files in sync. It includes a CLI,
[VS Code/Cursor extension](../renium-vscode-extension/readme.md), and
[Studio plugin](../plugin_ws_bridge/README.md).

## Install

Get the installer from [Releases](https://github.com/Superwheat/renium/releases/latest).

- **Windows:** run `Install-Renium.cmd`.
- **macOS/Linux:** extract the matching ZIP and run `./install.sh`.

Restart the editor and Studio. On macOS, use `~/Applications/Renium Studio.app`
for protected-property support; capture/input features may need Screen Recording
or Accessibility permission. Linux supports offline project tools; Studio features
require Windows or macOS.

`rbx upd` updates installed components. `rbx setup --repair` repairs the plugin.

## Start syncing

Open your place in Studio. For a new project, use a **dedicated empty folder**:

```powershell
rbx pl       # Studio → files
rbx lon      # Start two-way Live Sync
```

Open that folder in your editor. Save code in the editor; build in Studio.
For an existing project, start with `rbx lon` from its place folder—don't pull
over existing work just to connect. The extension offers the same controls.

```powershell
rbx status   # Studio connection and play state
rbx lst      # Live Sync state
rbx lof      # Stop Live Sync
```

Renium manages its background process and ports. No manual daemon setup is needed.

### Conflicts

Live Sync compares both sides against their last common state. One-sided changes
transfer, independent changes merge, and conflicting changes wait for a choice.
The editor prompts; the CLI returns resolution commands.

```powershell
rbx cfg set liveSync.initialSyncPriority reconcile
rbx cfg set liveSync.initialConflictPreference none
```

Initial modes are `reconcile` (apply) and `verify` (report only).
Conflict preferences are `none`, `studio`, or `editor` (files). A preference
resolves ordinary conflicts, not direct PackageLink edits.

### Manual sync and failures

```powershell
rbx pl
rbx ps src/ServerScriptService/Main.server.luau --verify
rbx ps src/ReplicatedStorage/Shared
```

Scope pushes to intended files/directories. An unfiltered push reconciles the
whole place and can remove Studio-only content; it is not a connection check.

Healthy Live Sync needs no push or repeated verification after each save.
Unsaved editor buffers do not sync. File edits during Play wait for Edit mode.

After a failure, inspect `rbx lst --details`, fix the cause, then `rbx rp` to
retry. `rbx dp` discards pending work. When the next operation needs completed
sync, `rbx lst --wait 10` waits up to ten seconds.

### Undo a sync

`rbx rev --sync latest` restores the last reconciled sync's affected files from
`.renium/editor-history/sync`. A push's `historyId` selects a specific sync.
Newer edits are protected: restore stops if the affected files have changed.
Live Sync transfers the restored files; otherwise add `--apply-studio`.
Add `--details` only when you need every restored path.

## Project files

```text
renium.project.jsonc
src/
  ServerScriptService/
    Main.server.luau
  ReplicatedStorage/
    Config.luau
instances/
  ServerScriptService.renium
  ReplicatedStorage.renium
sourcemap.json
```

Scripts are normal files. Service stores hold instances, properties, attributes,
and references; edit them through Renium's Explorer or CLI. Sourcemaps are generated.
The project config supports custom source roots, mounts, adapters, and filters.

Keep `instances/` in version control with your scripts. Renium automatically moves
older stores out of the source folders without changing their bytes. If both
locations contain different data, migration stops and preserves both copies.
Each place has its own `instances/`, even with a custom `sourceRoot`.
The separate `.renium/` directory contains local cache and undo data.

The default script layout remains unchanged. To opt into client/server folders,
add these mappings to `renium.project.jsonc`:

```jsonc
{
  "schemaVersion": 1,
  "tree": {
    "ServerScriptService": { "$path": "src/server" },
    "StarterPlayer": {
      "StarterPlayerScripts": {
        "$className": "StarterPlayerScripts",
        "$path": "src/client"
      }
    },
    "ReplicatedStorage": { "$path": "src/shared" }
  }
}
```

Use `Main.server.luau` in `src/server`, `Main.client.luau` in `src/client`,
and module scripts such as `Config.luau` in `src/shared`. Mappings choose the
Roblox parent; file suffixes choose script types. Mapped instance stores also
live under `instances/`, following their Roblox target path.

```powershell
rbx init my-place --with git,wally,selene,docs
rbx pv
rbx build -o build/place.rbxl
```

`init` preserves existing files; `--preview` shows proposed additions.
`pv` validates project configuration and mappings, not script syntax.
`build` creates a place without opening or publishing it.

Check script syntax without Studio or code execution:

```powershell
rbx ck src/ReplicatedStorage/Config.luau src/ServerScriptService/Main.server.luau
```

`ck` (`check`) accepts files or `-` for UTF-8 stdin, reports errors per file,
and exits nonzero if any fail. It does not check types or behavior.
Use project checks for those; don't use `loadstring` or enable it in Studio for validation.

For multiple places, work in the place folder or use
`rbx --place <alias|placeId> COMMAND`. Studio targets also accept
`gameId:placeId`. `rbx cs` lists connected Edit/server/client runtimes.

## Network simulation

For network testing during Play:

```powershell
rbx net presets
rbx net set --player 1 --preset mid
rbx net set --player 2 --preset poor
rbx net set --player 1 --in-delay 75 --out-jitter 20
rbx net restore --player 1
```

`normal`, `mid`, `high` and `poor` are editable starting templates, not guaranteed ping measurements. Each client can use different settings without restarting Play. Commands return the applied values. [Network settings, ranges, templates and cleanup](renium-guides/playtest.md#network-simulation).

## Read and edit data

Saved-data commands work without Studio:

```powershell
rbx f Workspace -n Door
rbx in Workspace -i editor:id
rbx bs Workspace -i editor:id -p Transparency --num 0.5
rbx ba Workspace -I editor:parent -n NewPart -c Part
rbx mv Workspace -i editor:id -I editor:parent
rbx ss DataStoreService UpdateAsync --limit 20
rbx sg RemoteEvent --limit 100
rbx q Place.rbxl -n RewardHandler
rbx cmp Place.rbxl
rbx cmp Before.rbxl --full --all
rbx cmp Before.rbxl --against After.rbxlx --full --all
```

Reuse returned IDs when names repeat. Live Sync sends changed paths automatically.
Without Live Sync, push returned paths with their settings IDs rather than the whole service.
`cmp` compares scripts by default; `--full` includes saved instances, properties, attributes and references. Input is before, project/`--against` is after. `--values` includes source and values, which may contain secrets. `v FILE --json` inspects models, places and Renium stores; use `cmp` for comparisons.

For bulk analysis, request fields once and process the result locally:

```powershell
'{"ops":[{"type":"search","q":"Door","limit":10,"fields":"lookup,prop:Anchored"}]}' | rbx bb Workspace -J -
```

Use live Luau only for unsaved Studio state or runtime APIs.
[Full data guide](renium-guides/data.md).

### Protected properties

```powershell
rbx access read Workspace StreamingEnabled
rbx access approve REQUEST_ID
rbx access write Workspace.Mesh CollisionFidelity Hull
```

On Windows and macOS Edit mode, `access` reads or writes properties blocked by
ordinary APIs. The default `ask` mode requires approval for the exact operation;
validated CollisionFidelity operations are allowlisted. `read-only` permits reads
but not protected writes. Unrestricted `read-write` requires explicit user opt-in.
[Modes, safety and supported values](renium-guides/data.md#protected-studio-properties).

## Roblox packages

Editing a linked package marks it **Changed** while preserving its PackageLink.
The result names affected packages, even if a later part of the edit fails.

```powershell
rbx pd ReplicatedStorage.SharedPackage   # Mark Changed
rbx pp ReplicatedStorage.SharedPackage   # Publish changes
rbx pu ReplicatedStorage.SharedPackage   # Discard changes and fetch latest
```

These target package roots on Windows/macOS. Use `--ords` for duplicate names
or a JSON array for path segments containing dots.
`upl` removes the PackageLink but keeps contents; it is not desync.
Publishing is a separate choice, never an automatic part of syncing.

## Verify without unnecessary playtests

Check saved code/data with offline queries, focused assertions, and the project's
tests first. Use Play only for a specific runtime question those checks cannot answer,
such as replication, input, or physics—not every edit or sync verification.
Reuse a suitable session and batch related checks.

```powershell
rbx play -s
rbx lc "return game.Players.LocalPlayer.Name" 1
rbx co --player 1 -n 20
rbx play -x
```

Ordinary Play is enough for one-client checks. `--players 2` starts a local server
and two clients. `l` targets Edit or the Play server; `lc` targets a client.

### Record and inspect

For an Edit-mode recording:

```powershell
rbx rs --studio -o clips/edit.mp4
rbx re
rbx rf clips/edit.mp4 --page 1
rbx rf clips/edit.mp4 --frame 15
```

`re` returns a timestamped overview PNG alongside the silent MP4.
An overview samples the clip; pages contain every consecutive captured frame.
Extract a full-resolution frame for detail. Review runs offline without FFmpeg
or scripts. [Capture guide](renium-guides/capture-device.md).

Device simulation changes viewport layout. Resource profiles limit CPU/memory;
they are approximate tiers, not hardware emulators, and cannot exceed the host.

### Investigate lag and network traffic

Built-in profiling is a trusted Renium workflow, separate from arbitrary
protected-property access. See the [performance guide](renium-guides/performance.md)
for runtime targeting, measurements and resource limits.

## Settings and references

```powershell
rbx cfg list --origins
rbx cfg get liveSync.initialSyncPriority
rbx cfg set liveSync.initialConflictPreference none
rbx cfg unset liveSync.initialConflictPreference
```

Settings list their current and allowed values. Writes target the active place;
`--scope user`, `workspace`, or `experience` selects a wider scope.
Precedence: user, workspace, experience, place, project, editor, CLI overrides.

Use `rbx` to list commands and `rbx COMMAND --help` for exact options.

| Workflow | Guide |
|---|---|
| Instances, scripts, batch reads/edits | [Data](renium-guides/data.md) |
| Pull, push, Live Sync | [Sync](renium-guides/sync.md) |
| Mounts, adapters, filters, Rojo import | [Configuration](renium-guides/configuration.md) |
| Models, place files, packages, links, Wally, Git | [Projects](renium-guides/projects.md) |
| Play, client/server Luau, console | [Playtests](renium-guides/playtest.md) |
| UI, input, movement | [Input](renium-guides/input.md) |
| Screenshots, recordings, device layout | [Capture](renium-guides/capture-device.md) |
| Lag spikes, MicroProfiler, replication, resource limits | [Performance](renium-guides/performance.md) |
| Open Cloud, publishing, Creator Store | [Cloud](renium-guides/opencloud.md) |
| Studio lifecycle and place management | [Advanced](renium-guides/advanced.md) |

Git commits, Studio sync, place builds, and Roblox publishing are separate operations.
Cloud commands need no Studio; supply credentials through environment variables,
not arguments. `rbx oc routes` lists cloud operations.

Agents read generated `RENIUM.md` and its guides. `init` adds pointers in
`AGENTS.md`/`CLAUDE.md` without replacing existing instructions.

## Troubleshooting

- **Disconnected:** `rbx status`; check the target place/plugin. Restart that
  Studio target after a plugin update.
- **Ambiguous runtime:** specify the place; `rbx cs` distinguishes Edit/server/client.
- **Pending sync:** `rbx lst --details`; resolve the cause instead of forcing a push/pull.
- **Installation/config:** `rbx dr --json`. `rbx dr --bundle diagnostics` creates
  a report; review it before sharing.

[Report bugs](https://github.com/Superwheat/renium/issues) with the version, OS,
command/editor action, and actual error.

Format limits: `TextChatMessage.Timestamp` and Studio-only `QDir`/`QFont` fields
are unsupported. Content supports URI/None; Object/Opaque sources stop export
instead of silently losing data. Infinity/NaN use tagged values. Script comparison
ignores line-ending-only differences.

## Plugins

Plugins add commands to Renium, from small helpers to whole workflows, without
changing its source.

```powershell
rbx plugin new my-plugin
# In my-plugin: cargo build --release
rbx plugin install ./my-plugin --dev
rbx my-plugin hello --name World
```

One manifest defines commands, arguments, help and time budgets; one Rust handler
implements them using the included SDK. No Renium source changes or separate SDK
install. `plugin list` shows installed plugins; `plugin info NAME` shows a plugin's
guide. Installation never builds or executes a plugin. Install only trusted native code.

[Authoring guide](renium-guides/plugins.md). The first plugin, [sandbox](../renium-plugins/sandbox/README.md),
gives a task an exclusive disposable Studio place; its source is separate and is not
included in Renium builds.

## Build

From the repository root:

```powershell
cargo build --locked --release --manifest-path tools/renium/Cargo.toml
cargo test --locked --manifest-path tools/renium/Cargo.toml
./tools/build-release.ps1 -LocalBuild
```

The last command bundles the CLI, extension, and plugin. Before replacing a locked
installed executable, stop Renium with `rbx dm stop --all`, not unrelated Studio processes.

[License](../../LICENSE)
