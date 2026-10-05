# Renium

Renium keeps a Roblox Studio place and a folder on your computer in sync, in both
directions. You can edit in Studio, in your editor, or in both at once, and each
side receives the other's changes. The whole place goes into the folder, not only
the scripts, so the map, UI, lighting and settings get Git history like code.

The same command, `rbx`, also operates Studio from a terminal. It starts playtests
with several clients, runs Luau on the server or on a client, reads the console,
sends input, takes screenshots and publishes. Because all of that is a terminal
command, an AI coding agent can build and test a game in Studio without someone
clicking for it.

Questions and help: [Discord](https://discord.gg/wwTFHSSNn3).
Bugs: [GitHub issues](https://github.com/Superwheat/renium/issues).

## If you use Rojo or Argon

Rojo and Argon treat your files as the source and send them to Studio. Renium
treats Studio and the files as two equal copies of the same place. In practice:

- **Studio edits come back to the files.** When you move a spawn, tune Lighting or lay out a UI in Studio, Renium writes the change into the project. With Rojo, that change stays only in the place until you rewrite it as a `.model.json` or `.meta.json` file, export a model file, or save the place and run `rojo syncback`.
- **The whole place is in the project.** A typical Rojo project keeps code in files and leaves the map in the place file. A Renium project holds the place's services with all their instances, so a clone of the repository can rebuild the place file without Studio (`rbx bep -o place.rbxl`).
- **Nothing is approximated.** Studio itself writes out the instances, and Studio reads them back in. Terrain, unions, mesh parts and saved properties that plugins cannot set come through unchanged. With Rojo or Argon, a non-script instance is only as complete as the model file you wrote or exported, and live sync can only apply what Roblox's plugin API allows.
- **Both sides can change while disconnected.** When you connect, Renium compares each side with the last state they had in common. Separate edits merge, and a real conflict waits for you to choose. Rojo and Argon resolve the first connection by letting one side replace the other.
- **There is nothing to start.** The first `rbx` command starts Renium's background process, and the Studio plugin connects to it by itself. With Rojo you run `rojo serve` and press Connect in the plugin.

## How it fits together

Renium has three parts, installed together:

- **The Studio plugin** watches the open place and applies changes that come from the files. Edits apply to the place you have open; restart Studio only after a plugin update.
- **The daemon** is a background process that `rbx` starts on first use. It holds the connection to Studio and watches the project folder, so Live Sync keeps running after the command that started it exits, and several terminals and the editor share one connection.
- **The `rbx` command and the VS Code extension** both talk to the daemon. The extension works in VS Code and Cursor and adds a project Explorer, a property Inspector, Live Sync controls, places, packages and a Git tab. `renium` is another name for `rbx`; both run the same program, and the docs use `rbx`.

A plugin cannot set some saved data directly, such as Terrain's voxel data or
`MeshPart.MeshId`. For those, Renium loads a small helper into the running Studio. It syncs Terrain,
reads and writes engine properties that scripts are blocked from (each such read
or write waits for your approval by default), publishes packages without dialogs,
and captures or sends input to play windows that are not in front. This works on
Windows and macOS. On macOS the installer adds a separate `Renium Studio` app for
it and leaves the original Studio app unchanged.

## What a project looks like

```text
my-game/
  renium.project.jsonc      settings and optional folder mappings
  src/
    ServerScriptService/Main.server.luau
    ReplicatedStorage/Config.luau
  instances/
    Workspace.renium
    ReplicatedStorage.renium
  RENIUM.md                 guide for AI agents, written by rbx init
  RENIUM/                   topic guides that RENIUM.md points to
  sourcemap.json            generated for luau-lsp
  .renium/                  local cache and sync history
```

- **`src/<Service>/...`** holds every script as a plain `.luau` file. The suffix sets the type: `.server.luau` is a Script, `.client.luau` a LocalScript and `.luau` a ModuleScript. Your editor, luau-lsp, Selene and Git treat them as ordinary text.
- **`instances/<Service>.renium`** holds everything else in that service: parts, models, UI, lights, values, Terrain, and the properties and attributes of the scripts. You do not write these files. Studio produces the data and Renium stores it on each sync. To change it, edit in Studio, in the extension's Explorer, or with commands such as `rbx bs` (set a property) that work while Studio is closed.
- **The stores are compressed binary**, so a text editor shows nothing useful. Run `rbx vci` once and `git diff` shows them as readable text and Git merges branches through Renium. `rbx vct FILE` prints one store as text.
- **`renium.project.jsonc`** holds settings and optional mappings in Rojo's `$path` form, such as `"ServerScriptService": { "$path": "src/server" }`.
- **An experience with several places** has `renium.experience.json` at the root and one project per place under `places/<alias>/`, each with its own `src/` and `instances/`. Commands run in a place folder target that place; elsewhere, add `--place <alias>`.

Commit `renium.project.jsonc`, `src/` and `instances/`. `.renium/` stays local.

## Compared side by side

| | Rojo | Argon | Renium |
|---|---|---|---|
| Files to Studio, live | Yes | Yes | Yes |
| Studio to files, live | Unstable option: script source and deletions | Off by default; properties opt-in | On by default, with properties |
| Both sides changed before connecting | One side wins | One side wins | Changes merge; conflicts wait for you |
| Saved place file into files | `rojo syncback` | No | `rbx pi` |
| Place file built without Studio | `rojo build` | `argon build` | `rbx bep` |
| Terrain and properties the plugin API refuses | Build a place file and open it | Not synced live | Synced live by the helper |
| Luau and playtests from a terminal | No | `argon exec`, `argon debug` | Edit, play server and each client |
| Runs on | Windows, macOS, Linux | Windows, macOS, Linux | Windows, macOS; Linux without Studio features |

The table reflects Rojo 7.7.0 and Argon 2.0.29 as of September 2026. Roblox's
built-in Script Sync needs no install and works with Team Create, but it covers
only scripts and folders.

### What it costs

- Other tools cannot read `.renium` stores. Rojo, Argon and editors without Renium see binary files; inside Git, `rbx vci` covers diffs and merges.
- Studio features need Windows or macOS. On Linux you get the offline tools, Open Cloud and plugins.
- Renium is younger than Rojo, with fewer users, tutorials and integrations. Adapters cover the common tools: `rbx wally` installs Wally packages, `rbx build` runs Wally and roblox-ts when a project uses them, `rbx init --with selene` adds a Selene config, and `rbx sm` writes a Rojo-style `sourcemap.json` for luau-lsp.
- The helper depends on Studio's internals. After a Studio update Renium finds them again by itself; when that fails, the affected command stops with an error until Renium is updated.

### Moving a Rojo project

In the Rojo project's folder, with the place open in Studio:

```powershell
rbx init
rbx lon
```

`rbx init` finds the Rojo project file, converts it into `renium.project.jsonc`
and adds the agent guides; `rbx ir --preview` shows the conversion without writing
it. Script suffixes, `init` scripts and `.meta.json`, `.model.json`, `.txt` and
`.csv` files build the same instances as in Rojo. On the first `rbx lon`, any
conflict between Studio and the files waits for you instead of being overwritten.

## Working with an AI agent

`rbx init` writes `RENIUM.md` and a `RENIUM/` folder of topic guides into the
project, and adds short pointers to it in `AGENTS.md` and `CLAUDE.md` without
removing what is already there. An agent such as Claude Code or Codex reads
`RENIUM.md` (about 1,200 tokens), opens a topic guide only when a task needs it,
and then works in Studio by running `rbx` in its terminal. It needs no MCP server
and loads no tool schemas.

The installer also adds a short note about `rbx` to the global instructions of the
agents it finds: Claude Code, Codex, Gemini CLI, OpenCode and Windsurf.

These are the commands an agent uses most. Each prints compact JSON, and output
from `print` comes back to the command instead of Studio's Output window.

| Task | Command |
|---|---|
| Run Luau in Edit, or on the server during Play | `rbx l "return workspace.Gravity"` |
| Run Luau on play client 1 | `rbx lc "return game.Players.LocalPlayer.Name" 1` |
| Start Play with a server and two clients | `rbx play -s --players 2` |
| Wait until something is true in the game | `rbx wait "workspace:GetAttribute('Ready')" -t 30` |
| Read the server's errors | `rbx co --server --level error` |
| Click and press keys in a client | `rbx inp -p 1 click Shop.BuyButton wait 100 key E` |
| Screenshot or record a window | `rbx sc --client -p 1 -o shot.png`, `rbx rs` then `rbx re` |
| Add latency and packet loss to one client | `rbx net set --player 1 --preset mid` |
| Raise render quality before a capture | `rbx gfx max` |
| Check Luau syntax without running it | `rbx ck` |
| Search, view or compare place files without Studio | `rbx q Place.rbxl -n Door`, `rbx v Model.rbxm`, `rbx cmp Before.rbxl --full` |
| Call Roblox Open Cloud | `rbx oc universe`, `rbx oc fetch` |
| Publish the open place | `rbx publish` |

Run `rbx` for the full list and `rbx COMMAND --help` for every option.

## Install

Download from [Releases](https://github.com/Superwheat/renium/releases/latest):

- **Windows (x64, ARM64):** run `Install-Renium.cmd`. It runs the PowerShell installer, which downloads the CLI, the Studio plugin (`Renium.rbxm`) and the extension for your editor, checks each download against the release's checksums, and puts `rbx` on PATH.
- **macOS (Apple Silicon, Intel):** extract the ZIP and open `Install Renium.command`.
- **Linux (x64, ARM64):** extract the ZIP and run `./install.sh`. Studio does not run on Linux, so Linux gets the offline tools, Open Cloud and plugins.

Restart your editor and Studio afterwards. Later, `rbx upd` updates the CLI, the
plugin and the extension together. Releases are built on the maintainer's own
machines, and release tags are signed.

## Get started

Open your place in Studio. In an empty folder for the project, run:

```powershell
rbx init
rbx pl
rbx lon
```

`rbx init` creates the project, `rbx pl` pulls the place into files and `rbx lon`
starts Live Sync. Open the folder in your editor and work on either side. For a
project that already has files, run only `rbx lon`, so the pull does not replace
your work.

## Documentation

- [CLI guide](tools/renium/README.md): install details, Live Sync, saved data, playtests, publishing, troubleshooting.
- [Topic guides](tools/renium/README.md#guides): the same guides `rbx init` gives agents.
- [VS Code/Cursor extension](tools/renium-vscode-extension/readme.md) and [Studio plugin](tools/plugin_ws_bridge).

## Build from source

```powershell
cargo build --locked --release --manifest-path tools/renium/Cargo.toml
```

The [CLI guide](tools/renium/README.md#build-from-source) has the full build and
bundle steps. Builds and release bundles are not stored in Git. Contributors should
read [AGENTS.md](AGENTS.md).

## Community

Questions, help and discussion happen in the [Discord server](https://discord.gg/wwTFHSSNn3),
where every release is announced. Bugs go to
[GitHub issues](https://github.com/Superwheat/renium/issues); `rbx report` prepares
a diagnostics zip to attach.

## License

Renium is licensed under the GNU Affero General Public License v3.0 with the
Commons Clause condition, which does not grant the right to sell the software. See
[LICENSE](LICENSE).
