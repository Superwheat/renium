# Renium

Two-way sync between Roblox Studio and a project folder, made for large places and
for working with AI coding agents.

## Why Renium

### It syncs what a plugin cannot

Rojo, Argon and the Studio MCP servers work through a Studio plugin. A plugin cannot:

- set `MeshPart.MeshId`,
- write Terrain the way Studio saves it,
- read or set the properties Roblox locks away from scripts.

Renium can. Beside its plugin it has a native helper that reads and writes a place
the way Studio itself saves it. A push puts back the whole place, not only the part
a script could set.

### The project is about the size of the place

Scripts are plain `.luau` files. Everything else is one compressed binary file per
service, for example `instances/Workspace.renium`.

| | Size |
|---|---|
| A place of 202,000 instances as an `.rbxl` | 37 MB |
| The same place as a Renium project | 41 MB (37 MB of stores, 4 MB of scripts) |

That is 18 store files instead of a file or a folder for every instance. With
Renium's Git setup they still show and merge as text.

### It stays fast on a big place

On that 202,000-instance place:

| | Time |
|---|---|
| Send the whole project to an empty Studio place | 11 s |
| Bring one property changed in Studio into the files | 4.5 s |

Times are from version 0.6.5 on a Windows desktop.

### Both sides can change

When the files and Studio were both edited since the last sync, Renium merges them:

- Edits to different instances, different properties or different lines of a script
  are all kept.
- Edits to the same value are reported with both versions. Nothing is overwritten
  until one of them is chosen.

### An agent works alone

Any MCP server can run a script in Studio. Renium takes away the points where a
person still had to step in:

- **Studio itself.** It opens, reopens, saves and closes places. It answers the
  prompts Studio puts in the way: auto-recovery, the lighting technology migration,
  the package-changes notice and the low-memory warning.
- **Several players.** A playtest runs a local server and as many clients as the
  test needs. Each client has its own Luau, console, input and capture.
- **Real input in the background.** Clicks, keys, typing and movement go to a play
  window that is not in front, so the agent tests while you keep using the PC.
- **What the player sees.** Each client can be screenshotted or recorded, also as an
  emulated phone or tablet, and the agent reads the frames.
- **Answers it can trust.** A push says what Studio now holds. Errors from the
  server and from each client come back to the agent.
- **Protected data.** Terrain, mesh ids and locked properties are written natively.
  Packages publish without a dialog.
- **Studio updates.** After a Studio update, Renium finds what it needs inside
  Studio again by itself.

## Install

Get the package for Windows, macOS or Linux from
[Releases](https://github.com/Superwheat/renium/releases/latest). It sets up
Renium, its Studio plugin and the VS Code or Cursor extension. An existing Rojo
project can be converted.

## The native helper

The helper does what a Studio plugin is not allowed to:

- write Terrain from the files into Studio,
- read and write locked properties,
- publish packages without dialogs,
- capture play windows that are not in front and send input to them.

By default, each read or write of a locked property waits for your approval.

How the helper gets into Studio:

- **Windows:** Renium loads it into the running Studio.
- **macOS:** Renium patches the installed Studio app. It renames the engine to
  `Contents/MacOS/RobloxStudio.bin` and installs its own launcher as
  `Contents/MacOS/RobloxStudio` with an ad-hoc signature. The original launcher and
  signature are kept under `~/Library/Application Support/Renium`, and
  `rbx setup --uninstall` restores them. A Studio update removes the patch until
  Renium applies it again.

The helper depends on Studio's internals. If Renium cannot find them after a Studio
update, the commands that need the helper stop with an error until Renium is
updated.

## Working with an AI agent

- Renium is one MCP tool whose input is an argument list, so an agent loads no tool
  schemas.
- The installer registers it for Claude Code, Codex, Cursor, Windsurf, Gemini CLI
  and OpenCode. It keeps a `.renium.bak` copy of each file it changes.
- A project carries `RENIUM.md` and a folder of topic guides. An agent reads about
  1,200 tokens and opens a guide only when a task needs it.

## Help

- **Questions:** the [Discord server](https://discord.gg/wwTFHSSNn3), where every
  release is announced.
- **Bugs:** [GitHub issues](https://github.com/Superwheat/renium/issues).
  `rbx report -m "what went wrong"` writes a diagnostics zip to attach.
- **Full guide:** [tools/renium/README.md](tools/renium/README.md) covers the project
  layout, Live Sync, saved data, playtests, publishing and building from source.
- **Editor extension:** see the
  [extension readme](tools/renium-vscode-extension/readme.md).
- **Contributing:** read [AGENTS.md](AGENTS.md).

## License

GNU Affero General Public License v3.0 with the Commons Clause condition, which does
not grant the right to sell the software. See [LICENSE](LICENSE).
