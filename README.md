# Renium

Renium connects Roblox Studio to a project folder on your computer. Scripts become
ordinary `.luau` files, the rest of the place becomes instance files you can edit,
diff and commit, and Live Sync keeps Studio and the folder matched in both
directions while you work in either one.

Renium also drives Studio itself: playtests with several clients, live Luau,
consoles, input, screenshots, packages and publishing. People use it through the
`rbx` command and a VS Code/Cursor extension, and AI coding agents use the same
commands. It is written mostly in Rust. Bug reports and suggestions are welcome.

## What Renium does better

### Sync that keeps both sides' work

Live Sync is a three-way comparison: it checks Studio and the files against the
last state they had in common. A change made on one side moves to the other,
separate changes merge, and a real conflict waits for your choice instead of one
side overwriting the other.

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
- Keep Studio's Undo from selecting and expanding every object it restores. Without this, undoing one sync selected 700 objects.
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

The official server works only with Studio's Edit session. Renium also reaches the
play server and every play client, in single-player and multiplayer tests: Luau and
consoles on each of them, and UI listing, input and screenshots on the clients.

The installer tells the agents it finds (Claude Code, Codex, Gemini CLI, OpenCode
and Windsurf) about `rbx`, and `rbx init` gives each project its own guide.

### Also included

- **Place files without Studio:** build a place (`rbx bep`), import one into files (`rbx pi`), search it (`rbx q`) and compare two versions property by property (`rbx cmp --full`).
- **Playtest tools:** latency, jitter and packet loss per client, frame-time and MicroProfiler captures, and CPU and memory limits for Studio on Windows.
- **Live collaboration:** `rbx collab` shares one project folder between several people's editors.
- **Open Cloud:** `rbx oc key add` stores API keys encrypted for your user account (DPAPI on Windows, the Keychain on macOS), `rbx oc games` finds the experiences a key can reach, and `rbx oc fetch` downloads one into a project.
- **Plugins:** add your own `rbx` commands, written in Rust.
- **Coming from Rojo:** `rbx ir` converts a Rojo project.

## Install

Download the installer from [Releases](https://github.com/Superwheat/renium/releases/latest):

- **Windows:** run `Install-Renium.cmd`.
- **macOS:** extract the ZIP and open `Install Renium.command`.
- **Linux:** extract the ZIP and run `./install.sh`. Linux gets the offline tools; Studio features need Windows or macOS.

The installer sets up the `rbx` command, the editor extension and the Studio
plugin. Restart your editor and Studio afterwards. `rbx upd` updates all three
together.

Releases are built on the maintainer's own machines, and release tags are signed.

## Get started

Open your place in Studio. In an empty folder for the project, run:

```powershell
rbx init
rbx pl
rbx lon
```

`rbx init` creates the project, `rbx pl` pulls the place into files and `rbx lon`
starts Live Sync. Open the folder in your editor and work on either side.

The [CLI guide](tools/renium/README.md) covers the rest.

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
