# Renium

Two-way sync between Roblox Studio and a project folder, made for large places and
for working with AI coding agents.

## Why Renium

### It syncs what a plugin cannot

Rojo, Argon and the Studio MCP servers reach Studio through a plugin, and a plugin
may only read and set what scripts are allowed to. The Terrain voxel grid as Studio
saves it, `MeshPart.MeshId` and every property scripts are blocked from stay out of
their reach.

Renium adds a native helper beside its plugin. It reads the place the way Studio
saves it and writes it back the same way, so a push rebuilds the place itself and
not the part of it a script can describe.

### The project is about the size of the place

Scripts are plain `.luau` files. Everything else is one compressed binary store per
service, such as `instances/Workspace.renium`: Studio's own saved data, packed in
columns.

| | Size |
|---|---|
| A place of 202,000 instances as an `.rbxl` | 37 MB |
| The same place as a Renium project | 41 MB (37 MB of stores, 4 MB of scripts) |

The stores are 18 files, one per service, instead of a file or a folder for each
instance. `rbx vci` makes Git show and merge a store as text.

### It stays fast on a big place

On that 202,000-instance place:

| | Time |
|---|---|
| Send the whole project to an empty Studio place | 11 s |
| Bring one property changed in Studio into the files | 4.5 s |

Times are from version 0.6.5 on a Windows desktop.

### Both sides can change

If files and Studio both changed since they last agreed, Renium compares each with
that last shared state:

- edits to different instances, different properties or different lines of a script
  are all kept,
- a real conflict is reported and waits for you to pick a side.

### Studio runs from a terminal

The same `rbx` command starts playtests, runs Luau on the server or a client, sends
input and takes screenshots. An AI coding agent can build a game and test it without
someone clicking for it, through its terminal or through one MCP tool.

## Install

Get the package for Windows, macOS or Linux from
[Releases](https://github.com/Superwheat/renium/releases/latest). It sets up the
`rbx` command, the Studio plugin and the VS Code or Cursor extension, and `rbx upd`
updates all three.

## How sync works

- **Scripts** are plain `.luau` files under `src/`.
- **Everything else** in a service, from parts and UI to Terrain and the scripts' own
  properties, goes into a store at `instances/<Service>.renium`.
- **Both directions.** Saved files go to Studio, and Studio edits come back as files.
- **Git.** The stores are compressed binary, so Rojo, Argon and editors without
  Renium cannot read them. Run `rbx vci` once and Git shows and merges them as text.
- **Rojo.** A Rojo project converts with `rbx init`, and `rbx ir --preview` shows the
  result first.

## The native helper

The helper does what a Studio plugin is not allowed to. It:

- writes Terrain from the files into Studio,
- reads and writes protected properties,
- publishes packages without dialogs,
- captures or sends input to play windows that are not in front.

Each protected read or write through the helper waits for your approval by default.

Editing voxels needs no helper. A script, or `rbx l --edit`, changes them through
the Terrain API (`FillBlock`, `WriteVoxels`) as in any plugin.

### How the helper gets into Studio

- **Windows:** Renium loads the helper into the running Studio.
- **macOS:** Renium patches the installed Studio app. `rbx setup`, which the
  installer runs, and `rbx upd` apply the patch, and opening Studio with `rbx so`
  checks it.

The macOS patch renames the engine to `Contents/MacOS/RobloxStudio.bin` and installs
Renium's launcher as `Contents/MacOS/RobloxStudio` with an ad-hoc signature. The
original launcher and code signature are kept under
`~/Library/Application Support/Renium`, so `rbx setup --uninstall` restores them. A
Studio update clears the patch until Renium applies it again.

### After a Studio update

The helper depends on Studio's internals. After a Studio update Renium finds them
again by itself. When that fails, the affected command stops with an error until
Renium is updated.

## Working with an AI agent

- `rbx init` writes `RENIUM.md` and a folder of topic guides into the project. An
  agent reads about 1,200 tokens and opens a guide only when a task needs it.
- The agent runs `rbx` in its terminal, or as one MCP tool whose input is the
  argument list, so it loads no tool schemas.
- The installer registers that tool and adds a short note for Claude Code, Codex,
  Cursor, Windsurf, Gemini CLI and OpenCode. It keeps a `.renium.bak` copy of each
  file it changes.

## Help

- **Questions:** the [Discord server](https://discord.gg/wwTFHSSNn3), where every
  release is announced.
- **Bugs:** [GitHub issues](https://github.com/Superwheat/renium/issues).
  `rbx report -m "what went wrong"` writes a diagnostics zip to attach.
- **CLI guide:** [tools/renium/README.md](tools/renium/README.md) covers the project
  layout, Live Sync, saved data, playtests, publishing, troubleshooting and building
  from source.
- **Editor extension:** the
  [extension readme](tools/renium-vscode-extension/readme.md) covers the editor side.
- **Contributing:** read [AGENTS.md](AGENTS.md).

## License

Renium is licensed under the GNU Affero General Public License v3.0 with the Commons
Clause condition, which does not grant the right to sell the software. See
[LICENSE](LICENSE).
