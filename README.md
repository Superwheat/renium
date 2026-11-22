# Renium

Two-way sync between Roblox Studio and a project folder, made for large places and
for working with AI agents.

## Why Renium

### It syncs what a plugin cannot

Rojo, Argon and the Studio MCP servers work through a Studio plugin, and a plugin
only gets what Roblox exposes to scripts. Renium has a native helper beside its
plugin and reaches the rest:

- **Everything Studio saves.** It reads a place the way Studio writes a place file.
  The project holds what scripts never see: collision meshes, union geometry, the
  Terrain grid, mesh ids.
- **Locked properties.** It reads and sets properties the scripting API refuses,
  such as `MeshPart.MeshId` and `CollisionFidelity`.
- **Roblox's own web APIs.** It sends requests to Roblox as the signed-in Studio
  user, through a service Roblox reserves for its own scripts. The APIs Studio
  itself uses, such as the creator inventory, are reachable without an API key.
- **The whole place at once.** A push hands Studio the place in Studio's own binary
  format. 200,000 instances arrive the way they do when Studio opens a place, not
  one script call at a time.

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

On that 202,000-instance place, with the most memory Renium used at each step:

| | Time | Memory |
|---|---|---|
| Check Studio and the files when nothing changed | 0.2 s | 0.5 GB |
| Send one changed script to Studio | 0.3 s | 0.6 GB |
| Send one changed property to Studio | 1.2 s | 1.0 GB |
| Bring one property changed in Studio into the files | 0.6 s | 0.6 GB |
| Bring the whole place from Studio into an empty project | 3.5 s | 0.8 GB |
| Send the whole project to an empty Studio place | 13 s | 1.8 GB |

No step needs 2 GB.

The same two directions with Argon 2.0.29:

| | Renium | Argon |
|---|---|---|
| Studio to files, 202,000 instances | 3.5 s | stopped with an error after 55 s |
| Files to Studio, 202,000 instances | 13 s | stopped out of memory at 6.9 GB |
| Studio to files, 20,000 instances of that place | 0.9 s | 31 s |
| Files to Studio, the same 20,000 instances | 3.3 s | 27 s |

Measured with version 0.6.5 on a Windows desktop.

### Both sides can change

When the files and Studio were both edited since the last sync, Renium merges them:

- Edits to different instances, different properties or different lines of a script
  are all kept.
- Edits to the same value come back as a list of conflicts. Each one carries the
  last synced value and both new ones, or both diffs for a script, so an agent
  decides each conflict by itself. Nothing is overwritten until every conflict has
  an answer.

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
- send requests to Roblox's web APIs as the signed-in Studio user,
- publish packages without dialogs,
- capture play windows that are not in front and send input to them.

By default, each read or write of a locked property and each such request waits
for your approval.

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
