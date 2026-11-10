# Renium CLI

`rbx` keeps a Roblox Studio place and a project folder in sync, edits the saved
place while Studio is closed, drives Studio and playtests from a terminal, and
calls Open Cloud. Installing, the first three commands, the native helper and agent
setup are in the [main README](../../README.md). This page covers day-to-day use;
`rbx` lists every command and `rbx COMMAND --help` shows its options.

## Setup

The installer runs `rbx setup`, which installs the Studio plugin and adds the
`rbx` note to agents' global instructions, such as `~/.claude/CLAUDE.md` and
`~/.codex/AGENTS.md`.
`rbx setup --status` reports the installation, `--repair` reinstalls the plugin and
`--uninstall` removes the note and the plugin. The project guides that `rbx init`
writes refresh themselves when Renium updates, and no other command creates a
project unasked.

## Live Sync

`rbx status` shows the connection to Studio, `rbx lst` shows Live Sync and
`rbx lof` stops it. A first-connection conflict waits for your choice: the editor
asks, the CLI prints the commands that resolve it, and `rbx lst --details` shows
both values. Unsaved editor buffers do not sync, and file edits made during Play
wait for Edit mode. While Live Sync reports no problem, there is no need to push,
poll or playtest after each save. After a failure, read `rbx lst --details`, fix the cause, then run `rbx rp`
to retry or `rbx dp` to discard.

Without Live Sync, push only what you changed:

```powershell
rbx ps src/ServerScriptService/Main.server.luau --verify-sources
rbx rev --sync latest
```

A push with no paths reconciles the whole place and can remove content that exists
only in Studio.
`rev` undoes the last sync by restoring the files it changed, and refuses if they
have changed since; Live Sync carries the result to Studio, or add `--apply-studio`.

## Project layout

```text
renium.project.jsonc
src/
  ServerScriptService/Main.server.luau
  ReplicatedStorage/Config.luau
instances/
  ServerScriptService.renium
  ReplicatedStorage.renium
sourcemap.json
```

The script suffix sets the type: `.server.luau` is a Script, `.client.luau` a
LocalScript and `.luau` a ModuleScript. Each `.renium` store holds the rest of its
service with properties, attributes and references. You never write a store by
hand; edit in Studio, in the extension, or with the saved-data commands below.
`rbx vct FILE` prints one store as text. Commit `renium.project.jsonc`, `src/` and
`instances/`; `sourcemap.json` is generated for luau-lsp, and `.renium/` holds
local cache and undo data.

An experience with several places has `renium.experience.json` at its root and one
project per place under `places/<alias>/`. Work from the place folder or put
`--place <alias>` before a command. `rbx cfg list --origins` shows every setting
and where its value comes from.

## Saved data

These commands work on the project files, so Studio can stay closed; Live Sync
sends the changes to Studio.

```powershell
rbx f Workspace -n Door
rbx bs Workspace Lobby.Door -p Transparency --num 0.5
```

A target is a dotted path such as `Lobby.Door`, or `-i ID`, `-n NAME` or
`-c CLASS`. Place files need no Studio either: `rbx q Place.rbxl -n Door` searches
one, `rbx cmp Before.rbxl --full` compares one with the project,
`rbx bep -o place.rbxl` builds one and `rbx pi Place.rbxl` imports one.

## Protected properties

```powershell
rbx access read Workspace StreamingEnabled
rbx access approve REQUEST_ID
```

`access` reads and writes engine properties that Studio's scripting API blocks, in
Edit mode, through the native helper. A read or write outside a short allowlist
runs only after you approve that exact operation, and each approval works once.
`rbx access mode read-only` refuses writes. The
[properties guide](renium-guides/data-properties.md) has the details.

## Studio and playtests

```powershell
rbx play -s --players 2
rbx lc "return game.Players.LocalPlayer.Name" 1
```

`play -s --players 2` starts a local server with two clients, and `play -x` stops
the test. `l` runs Luau in Edit or on the server during Play, and `lc` runs it on
a client. `co` reads a console, `wait` waits for a Luau condition, `inp` sends keys
and clicks, and `sc` and `rs` capture screenshots and recordings, all without
taking focus. `ro` reopens the remembered place and `so` opens a file, also without
taking focus, and `sx` closes Studio.

## Packages, publishing and Git

```powershell
rbx pp ReplicatedStorage.SharedPackage
rbx publish --dry-run
```

`pp` publishes a linked package without dialogs, and an edit inside one marks it
Changed while keeping its PackageLink. `publish` sends the open Edit session to
its existing place with your Studio login once Live Sync has settled; it refuses
during Play or while file changes are still pending. `--open-cloud` builds and
uploads the project with an API key instead. Read the
[publishing guide](renium-guides/publishing-validation.md) before an Open Cloud
upload, because some instance types cannot be updated that way. If a publish is
not confirmed, check Version History before retrying. Sync never publishes, and
neither does a Git commit or push.

`rbx vci` sets up Git to show `.renium` stores as text in `git diff` and to merge
them through Renium. `rbx collab start` shares the project live with other editors.
`rbx oc key add studio` stores an Open Cloud key from a hidden prompt (DPAPI on
Windows, the Keychain on macOS), so keys stay out of arguments and project files.

## Plugins

`rbx plugin new my-plugin` starts a plugin in Rust with the included SDK, and the
[plugins guide](renium-guides/plugins.md) covers building and installing one.
Plugins are native code with your account's access, so install only ones you trust.

## Guides

Agents read the same guides, starting from the [agent guide](renium-agents.md).

| Topic | Guide |
|---|---|
| Pull, push, Live Sync, conflicts, undo | [Sync](renium-guides/sync.md) |
| Saved instances, scripts, bulk queries, protected properties | [Data](renium-guides/data.md) |
| Layout, mappings, mounts, adapters, filters, Rojo import | [Configuration](renium-guides/configuration.md) |
| Models, place files, packages, links, Wally, Git, collaboration | [Projects](renium-guides/projects.md) |
| Play, live Luau, consoles, network simulation | [Playtests](renium-guides/playtest.md) |
| UI, input, movement | [Input](renium-guides/input.md) |
| Screenshots, recordings, device simulation | [Capture](renium-guides/capture-device.md) |
| Lag spikes, MicroProfiler, resource limits | [Performance](renium-guides/performance.md) |
| Open Cloud, API keys, creator assets | [Cloud](renium-guides/opencloud.md) |
| Studio lifecycle, places, audio, publishing | [Advanced](renium-guides/advanced.md) |
| Using and writing plugins | [Plugins](renium-guides/plugins.md) |

## Troubleshooting

`rbx status` names the reason Studio is not connected: Studio closed, plugin
missing, plugin waiting for a Studio restart, plugin not connecting, or another
place open. With several Studios or places open, `rbx cs` lists the runtimes and
`--place` picks one. `rbx dr --json` checks the installation and configuration.

When something in Renium itself breaks, run `rbx report -m "what went wrong"`. It
writes a zip with the diagnostics, the daemon log and the recent `rbx` commands
from your agent's transcript, with keys and home paths masked. Review it, then
attach it to the [GitHub issue](https://github.com/renium-rbx/renium/issues) link it
prints. Questions go to the [Discord server](https://discord.gg/wwTFHSSNn3).

## Build from source

Run Cargo from `tools/renium` so it uses the pinned Rust toolchain:

```powershell
cd tools/renium
cargo build --locked --release
cargo test --locked
cd ../..
./tools/build-release.ps1 -LocalBuild
```

The last command bundles the CLI, extension and plugin under `dist/`. To replace
an installed executable that is in use, stop Renium with `rbx dm stop --all` rather
than closing Studio.

[License](../../LICENSE)
