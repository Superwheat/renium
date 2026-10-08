# Renium CLI

`rbx` keeps a Roblox Studio place and a project folder in sync in both
directions, edits the saved place while Studio is closed, drives Studio and
playtests from a terminal, and calls Open Cloud. `renium` is another name for the
same program; the docs use `rbx`. The
[VS Code/Cursor extension](../renium-vscode-extension/readme.md) and the
[Studio plugin](../plugin_ws_bridge/README.md) install with it.

Run `rbx` for the command list and `rbx COMMAND --help` for every option.

## Install

Download from [Releases](https://github.com/Superwheat/renium/releases/latest):

- **Windows:** run `Install-Renium.cmd`. It runs the PowerShell installer, which picks the x64 or ARM64 build and checks each download against the release's checksums.
- **macOS:** extract the ZIP and open `Install Renium.command`, or run `./install.sh`. Use `~/Applications/Renium Studio.app` for protected properties; the original Studio app is unchanged. Capture and input may need Screen Recording or Accessibility permission.
- **Linux:** extract the ZIP and run `./install.sh`. Studio does not run on Linux, so Linux gets the offline tools, Open Cloud and plugins, but nothing that needs Studio.

The installer asks which editor should get the extension, puts `rbx` on PATH and
installs the Studio plugin. Restart the editor and Studio afterwards. Later,
`rbx upd` updates all three together and `rbx setup --repair` reinstalls the plugin.

### Agents

The installer runs `rbx setup`, which adds a short note about `rbx` to the global
instructions of the agents it finds: Claude Code (`~/.claude/CLAUDE.md`), Codex
(`~/.codex/AGENTS.md`), Gemini CLI, OpenCode and Windsurf. `rbx setup --status`
reports the installation; `rbx setup --uninstall` removes the note and the plugin.

In a project, `rbx init` writes `RENIUM.md` and a `RENIUM/` folder of topic
guides, plus pointers in `AGENTS.md` and `CLAUDE.md` for Cursor, Claude Code, Codex
and Copilot, keeping any instructions already there. An agent that opens the
project reads `RENIUM.md`, opens a topic guide when a task needs it, and then
operates Studio by running `rbx` in its terminal. The guides refresh themselves
when Renium updates. Other commands never create a project unasked, and the editor
extension offers to initialize a folder that has none.

## First sync

Open the place in Studio. For a new project, start in an empty folder:

```powershell
rbx init
rbx pl
rbx lon
```

`init` creates the project, `pl` pulls the place into files and `lon` starts Live
Sync. Open the folder in your editor: saved files go to Studio and Studio edits come
back as files. The extension has the same controls.

For an existing project, run only `rbx lon`; a pull would replace your files with
Studio's copy. To start from a place file, run `rbx init` in its folder, open the
file with `rbx so Place.rbxl`, then pull. `rbx status` shows the connection,
`rbx lst` shows Live Sync and `rbx lof` stops it.

There is no server to start. The first `rbx` command starts Renium's background
process (the daemon), and the Studio plugin connects to it by itself. The daemon
keeps Live Sync running after `rbx lon` exits and lets the editor and every
terminal share one connection to Studio.

## Live Sync

Live Sync compares Studio and the files with the last state they had in common.
One-sided changes transfer, separate changes merge, and a conflict waits for your
choice instead of overwriting either side. This matters most on first contact:
if Studio and the files both changed while Live Sync was off, connecting keeps
both sets of edits instead of making one side match the other. The editor asks;
the CLI prints the commands that resolve it, and `rbx lst --details` shows both
values. Setting
`liveSync.initialConflictPreference` to `studio` or `editor` settles ordinary
first-connection conflicts toward that side.

- Unsaved editor buffers do not sync, and file edits made during Play wait for Edit mode.
- While Live Sync reports no problem, there is no need to push, poll or playtest after each save.
- After a failure, read `rbx lst --details`, fix the cause, then `rbx rp` to retry or `rbx dp` to discard.

Without Live Sync, push only what you changed; `--verify` confirms the script
source in Studio. A push with no paths reconciles the whole place and can remove
content that exists only in Studio.

```powershell
rbx ps src/ServerScriptService/Main.server.luau --verify
rbx rev --sync latest
```

`rev` undoes the last sync by restoring the files it changed from
`.renium/editor-history/sync`, and refuses if they have changed since. Live Sync
carries the result to Studio; otherwise add `--apply-studio`.

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

Scripts are ordinary `.luau` files, so editors, luau-lsp, Selene and Git read
them as text. The suffix sets the type (`.server.luau`, `.client.luau`, or `.luau`
for a ModuleScript).

Each `instances/<Service>.renium` store holds the rest of that service: every
non-script instance with its properties, attributes and references, Terrain, and
the properties and attributes of the scripts. Renium writes the stores from what
Studio itself saves, so anything Studio can save in a place file, such as Terrain
voxels, unions and mesh data, is kept and restored instead of being reduced to what
a hand-written model file can describe. You never write a store by hand: edit in
Studio, in the extension, or with the saved-data commands below. The stores are
compressed binary; `rbx vct FILE` prints one as text, and `rbx vci` makes Git show
and merge them as text.

Commit `renium.project.jsonc`, `src/` and `instances/`. `sourcemap.json` is
generated for luau-lsp, and `.renium/` holds local cache and undo data.

An experience has `renium.experience.json` at its root and one project per place
under `places/<alias>/`, each with its own `src/` and `instances/`; work from the
place folder or put `--place <alias|placeId>` before a command. The
[configuration guide](renium-guides/configuration.md) covers client and server
folders, mounts, adapters, filters and Rojo import (`rbx ir`). `rbx pv` validates
the configuration and `rbx ck FILE...` checks Luau syntax, both offline.

`rbx cfg list --origins` shows every setting, its allowed values and where its
value comes from. `rbx cfg set KEY VALUE` writes to the active place; add
`--scope user`, `workspace` or `experience` to apply it more widely.

## Saved data

These commands work on the project files, so Studio can stay closed; Live Sync
sends the changes to Studio.

```powershell
rbx f Workspace -n Door
rbx bs Workspace Lobby.Door -p Transparency --num 0.5
```

A target is a dotted path (`Lobby.Door`, or `Borders.Border[4]` for a duplicate
name), `-i ID`, `-n NAME` or `-c CLASS`. `in` inspects an instance, `bg` reads a
property, `ba` adds an instance and `sg` searches script lines. `bb` answers many
field queries in one JSON request.

Place files need no Studio either: `rbx q Place.rbxl -n Door` searches one (`--props Anchored,Size` adds saved property values), `rbx cmp Before.rbxl --full` compares every saved instance and property with
the project (or `--against` another file), `rbx bep -o place.rbxl` builds one and
`rbx pi Place.rbxl` imports one into the project.

## Protected properties

```powershell
rbx access read Workspace StreamingEnabled
rbx access approve REQUEST_ID
```

`access` reads and writes engine properties that Studio's scripting API blocks, in
Edit mode on Windows and macOS. A plugin cannot reach these at all; Renium reaches
them through a helper it loads into the running Studio. By default, a read or
write outside a short allowlist runs only after you approve that exact operation,
and each approval works once. One read can list up to 64 instances after the property under a single
approval. `rbx access mode read-only` refuses writes. See the
[data guide](renium-guides/data.md#protected-studio-properties) for details.

## Studio and playtests

```powershell
rbx play -s --players 2
rbx lc "return game.Players.LocalPlayer.Name" 1
```

`play -s` starts Play and `play -x` stops it; `--players 2` starts a local server
with two clients. During such a test, `play --add-players N` adds clients and
`play --leave -p N` closes one without ending the session. `l` runs Luau in Edit, or on the server during Play, and `lc`
runs it on a client. `co` reads a console, `wait` waits for a Luau condition,
`inp` sends keys and clicks to a client, and `sc` and `rs` capture screenshots and
recordings, all without taking focus. Printed output comes back to you instead of
Studio's Output, so a script or agent can read what happened. Check saved code
with offline queries and tests first, start Play only for runtime questions such
as input, replication or physics, and reuse a running session.

`ro` reopens the remembered place and `so` opens a file, both without Studio taking
focus; `sx` closes Studio. `rbx audio auto --global` mutes Studio while it is not
focused, leaving game sounds unchanged.

## Packages and publishing

```powershell
rbx pp ReplicatedStorage.SharedPackage
rbx publish --dry-run
```

An edit inside a linked package marks it Changed first and keeps its PackageLink;
the result lists those packages in `autoDesyncedPackages`, even when the edit
fails. `pd` marks a package Changed, `pp` publishes it and `pu` discards changes
and fetches the published version, without dialogs or focus. They wait up to two
minutes (`--timeout` up to 600 seconds).

`publish` sends the selected Studio Edit session to its existing place with your
Studio login once Live Sync has settled; it refuses during Play or while file
changes are still pending (`--allow-pending` publishes Studio as it is). When the place
refuses the save API, Renium uses Studio's own Publish command. `--open-cloud`
builds and uploads the project with an API key instead. If a publish is not
confirmed, check Version History before retrying. Nothing is published as part of
sync.

## Git, collaboration and Open Cloud

```powershell
rbx vci
rbx oc key add studio
```

`vci` sets up Git to diff `.renium` stores as text and merge them with Renium, so
`git diff` and `git log -p` show which instances and properties changed, and two
branches that edited the same store merge instead of stopping at a binary
conflict. A commit or push never publishes to Roblox. `rbx collab start` shares
the project live with other editors and prints an invite. `oc key add` stores an API key from a
hidden prompt (DPAPI on Windows, the Keychain on macOS), and every `oc` command uses
it when `ROBLOX_API_KEY` is unset, so keys stay out of arguments and project files.

## Plugins

`rbx plugin new my-plugin` starts a plugin: one manifest for its commands and a
Rust handler using the included SDK. Build it with `cargo build --release` in its
folder, then `rbx plugin install ./my-plugin --dev`. Installing never builds or runs it.
Plugins are native code with your account's access; install only ones you trust.

## Guides

Agents read the same guides, starting from the [agent guide](renium-agents.md).

| Topic | Start with | Guide |
|---|---|---|
| Pull, push, Live Sync, conflicts, undo | `rbx lon` | [Sync](renium-guides/sync.md) |
| Saved instances, scripts, bulk queries, protected properties, place comparison | `rbx f`, `rbx bb` | [Data](renium-guides/data.md) |
| Layout, mappings, mounts, adapters, filters, Rojo import | `rbx cfg list` | [Configuration](renium-guides/configuration.md) |
| Models, place files, packages, links, Wally, Git, live collaboration | `rbx bep`, `rbx collab` | [Projects](renium-guides/projects.md) |
| Play, live Luau, consoles, network simulation | `rbx play -s`, `rbx net` | [Playtests](renium-guides/playtest.md) |
| UI, input, movement | `rbx ui`, `rbx inp` | [Input](renium-guides/input.md) |
| Screenshots, recordings, device simulation | `rbx sc`, `rbx rs` | [Capture](renium-guides/capture-device.md) |
| Lag spikes, MicroProfiler, resource limits | `rbx perf`, `rbx pf` | [Performance](renium-guides/performance.md) |
| Open Cloud, API keys, creator assets | `rbx oc` | [Cloud](renium-guides/opencloud.md) |
| Studio lifecycle, places, audio, publishing | `rbx ro`, `rbx publish` | [Advanced](renium-guides/advanced.md) |
| Using and writing plugins | `rbx plugin` | [Plugins](renium-guides/plugins.md) |

## Troubleshooting

- **No Studio connection:** `rbx status` names the cause: Studio closed, plugin missing, plugin needs a Studio restart, plugin not connecting, or another place open. Restart Studio after a plugin update.
- **Several Studios or places:** `rbx cs` lists Edit, server and client runtimes; add `--place` to pick one.
- **Sync failed or pending:** read `rbx lst --details` and fix the cause instead of forcing a push or pull.
- **Installation or configuration:** run `rbx dr --json`.
- **Something in Renium itself broke:** run `rbx report -m "what went wrong"`. It writes a zip with the diagnostics, the daemon log and the recent `rbx` commands from your agent's transcript, with keys and home paths masked; review it, then attach it to the issue link it prints.

[Report a bug](https://github.com/Superwheat/renium/issues) with the Renium version,
your OS, the command or editor action, and the exact error. For questions and help,
join the [Discord server](https://discord.gg/wwTFHSSNn3).

## Build from source

Run Cargo from `tools/renium` so it uses the pinned Rust toolchain:

```powershell
cd tools/renium
cargo build --locked --release
cargo test --locked
cd ../..
./tools/build-release.ps1 -LocalBuild
```

`cargo build --profile fast` skips link-time optimization and takes about half
the time of a release build. The last command bundles the CLI, extension and plugin
under `dist/`. To replace an installed executable that is in use, stop Renium with
`rbx dm stop --all` rather than closing Studio.

[License](../../LICENSE)
