# Renium

Renium connects Roblox Studio to a project folder on your computer. Scripts become
ordinary `.luau` files, the rest of the place becomes instance files you can edit,
diff and commit, and Live Sync keeps Studio and the folder matched in both
directions while you work in either one.

Renium also drives Studio itself: playtests with several clients, live Luau,
consoles, input, screenshots, packages and publishing. People use it through the
`rbx` command and a VS Code/Cursor extension, and AI coding agents use the same
commands. It is written mostly in Rust. Bug reports and suggestions are welcome.

## Compared with Rojo and other sync tools

Rojo is the most widely used way to keep a Roblox project in files. It is mature,
roblox-ts lists it as a prerequisite, luau-lsp reads its sourcemaps, and Wally,
Selene and Rokit round out the usual setup. Rojo treats the files as the source of
truth and stops at sync. Renium keeps both sides editable, then drives Studio.

| | Rojo | Argon | Studio Script Sync | Renium |
|---|---|---|---|---|
| Files to Studio, live | Yes | Yes | Scripts and folders | Yes |
| Studio to files, live | Unstable option: script source and deletions | Off by default; properties opt-in | Scripts and folders | On by default, with properties |
| Both sides changed before connecting | One side wins | One side wins | Keep Studio or Keep Disk | Changes merge; conflicts wait for you |
| Saved place file into files | `rojo syncback` | Not yet | No | `rbx pi` |
| Place file built without Studio | `rojo build` | `argon build` | No | `rbx bep` |
| Terrain and properties the plugin API refuses | Build a place file and open it | Not synced live | No | Written by a helper inside Studio |
| Luau and playtests from a terminal | No | `argon exec`, `argon debug` | No | Edit, server and each client |
| Runs on | Windows, macOS, Linux | Windows, macOS, Linux | Inside Studio | Windows, macOS; Linux without Studio features |

Script Sync needs no install and works with Team Create, which suits teams that
only want code in an editor. The table reflects Rojo 7.7.0, Argon 2.0.29 and
Roblox's Script Sync documentation as of September 2026.

### What a Rojo user stops doing

- **Rebuilding Studio edits in files.** When someone moves a spawn, tunes Lighting or lays out a UI in Studio, Rojo keeps that change only in the place. Getting it into the project means rewriting it as `.model.json` or `.meta.json`, exporting a model file, or saving the place and running `rojo syncback`. Renium writes it to the project as it happens and merges it with file edits made meanwhile.
- **Keeping the world outside Git.** In what Rojo calls a partially managed project, only code lives in files. Renium keeps the whole place in the project, so the map, UI and settings get history, diffs and merges like code.
- **Opening built place files for what the plugin API refuses.** Rojo's documentation lists Terrain and `MeshPart.MeshId` among the things it cannot sync live. Renium syncs both into the open place, and reads or writes other blocked engine properties with your approval.
- **Checking results by hand.** Rojo's job ends at sync, so you start Play and read Output yourself. Renium starts local tests with several clients, runs Luau on the server and each client, reads their consoles, sends input and takes screenshots, so you or an agent can inspect a running test instead of restarting it to look.

### What changes and what it costs

- Renium keeps instance data in binary `.renium` stores under `instances/` rather than in text model files. `rbx vci` makes Git diff and merge them as text, but other tools cannot read them.
- Studio features need Windows or macOS. On Linux, Renium offers the offline tools, Open Cloud and plugins.
- Renium is younger than Rojo, with fewer users, tutorials and integrations. Adapters cover the common tools: `rbx wally` installs Wally packages into the project, `rbx build` runs Wally and roblox-ts when a project uses them, `rbx init --with selene` adds a Selene config, and `rbx sm` writes a Rojo-style `sourcemap.json` for luau-lsp.
- The helper depends on Studio's internals. After a Studio update Renium finds them again by itself; when that fails, the affected command stops with an error until Renium is updated.

### Moving a Rojo project

In the Rojo project's folder, with the place open in Studio:

```powershell
rbx init
rbx lon
```

`rbx init` finds the Rojo project file, converts it into `renium.project.jsonc` and
adds Renium's guides; `rbx ir --preview` prints the conversion on its own. Script
suffixes, `init` scripts and `.meta.json`, `.model.json`, `.txt` and `.csv` files
build the same instances as in Rojo. On the first `rbx lon`, conflicts wait for you.

## What Renium does better

### Sync that keeps both sides' work

Live Sync checks Studio and the files against the last state they had in common. A
change on one side moves to the other, separate changes merge, and a real conflict
waits for your choice instead of one side overwriting the other.

- Every reconciled sync saves the files it replaced, and `rbx rev --sync latest` puts them back.
- An edit inside a linked Roblox package marks the package Changed before changing its contents, and keeps its PackageLink.
- `rbx vci` sets up Git to show instance files as readable text and to merge them.

### Goes further than a Studio plugin can

A Studio plugin can only do what Roblox's plugin API allows. Renium also loads a
small helper into the running Studio, which lets it do things that API refuses:

- Read and change engine properties that scripts are blocked from, with each read or write waiting for your approval by default.
- Write data that scripts cannot, such as custom audio attenuation curves, and sync Terrain.
- Mark Roblox packages Changed, publish them and update them without dialogs and without taking focus.
- Publish a place through Studio's own Publish command when the place refuses the save API.
- Keep Studio's Undo from selecting and expanding every object it restores.
- Mute Studio from inside Studio, leaving the system volume mixer alone.
- Open Studio without it jumping in front of your work, take screenshots of play windows that are not in front, and send input to play clients in the background.

All of this works on Windows and macOS. On macOS the installer adds a separate
Renium Studio app for it and leaves the original Studio app unchanged.

### Fewer tokens for AI agents

An agent reads one short guide when it starts and opens a topic guide only when a
task needs it, and command output is kept compact. Measured against Roblox's
official Studio MCP server on the same place (about 68,000 instances), running the
same operations, with token counts from Codex's tokenizer:

| Operation | Studio MCP | Renium |
|---|---|---|
| Fixed cost per session | 13,880 (tool schemas) | 1,129 (agent guide), plus about 1,000 per guide the agent opens |
| Extra cost on every call | 23 (Studio UUID) | none |
| Inspect a Model | 464 | 48 compact, 109 full |
| Inspect a MeshPart with every property | 896 | 397 |
| Run a line of Luau | 99 | 55 |
| Read the console | 405 | 246 |
| Grep scripts, per matching line | 35 | 32 |

Claude's tokenizer gives 5 to 20 percent higher counts for both, with the same ranking.

The official server can start Play, run Luau in Edit or on the play server and
client, and send input to that client. Renium also runs local tests with several
clients and reaches each one: Luau, consoles, UI listing, input and screenshots.

The installer tells the agents it finds (Claude Code, Codex, Gemini CLI, OpenCode
and Windsurf) about `rbx`, and `rbx init` gives each project its own guide.

### Also included

- **Place files without Studio:** build a place (`rbx bep`), import one into files (`rbx pi`), search it (`rbx q`) and compare two versions property by property (`rbx cmp --full`).
- **Playtest tools:** latency, jitter and packet loss per client, frame-time and MicroProfiler captures, and CPU and memory limits for Studio on Windows.
- **Live collaboration:** `rbx collab` shares one project folder between several people's editors.
- **Open Cloud:** `rbx oc key add` stores API keys encrypted for your user account (DPAPI on Windows, the Keychain on macOS), `rbx oc games` finds the experiences a key can reach, and `rbx oc fetch` downloads one into a project.
- **Plugins:** add your own `rbx` commands, written in Rust.

## Install

Download the installer from [Releases](https://github.com/Superwheat/renium/releases/latest):

- **Windows:** run `Install-Renium.cmd`.
- **macOS:** extract the ZIP and open `Install Renium.command`.
- **Linux:** extract the ZIP and run `./install.sh`. Linux gets the offline tools; Studio features need Windows or macOS.

The installer sets up the `rbx` command, the editor extension and the Studio
plugin. Restart your editor and Studio afterwards. `rbx upd` updates all three
together. Releases are built on the maintainer's own machines, and release tags
are signed.

## Get started

Open your place in Studio. In an empty folder for the project, run:

```powershell
rbx init
rbx pl
rbx lon
```

`rbx init` creates the project, `rbx pl` pulls the place into files and `rbx lon`
starts Live Sync. Open the folder in your editor and work on either side. The
[CLI guide](tools/renium/README.md) covers the rest.

## Components

- [CLI](tools/renium): sync, project tools and Studio automation.
- [VS Code/Cursor extension](tools/renium-vscode-extension): Explorer, properties, Live Sync, places, packages and Git.
- [Studio plugin](tools/plugin_ws_bridge): the connection inside Studio.

## Build from source

```powershell
cargo build --locked --release --manifest-path tools/renium/Cargo.toml
```

The [CLI guide](tools/renium/README.md#build-from-source) has the full build and
bundle steps. Builds and release bundles are not stored in Git. Contributors should
read [AGENTS.md](AGENTS.md).

## License

Renium is licensed under the GNU Affero General Public License v3.0 with the
Commons Clause condition, which does not grant the right to sell the software. See
[LICENSE](LICENSE).

[Report a bug](https://github.com/Superwheat/renium/issues)
