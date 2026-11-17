# Renium

Renium keeps a Roblox Studio place and a folder on your computer in sync, in both
directions.

- **The whole place is in the folder**, not only the scripts. The map, UI, lighting
  and settings get Git history like code.
- **Studio runs from a terminal.** The same `rbx` command runs playtests, Luau, input
  and screenshots, so an AI coding agent can build and test a game without someone
  clicking for it.

## Install

Download the package for your system from
[Releases](https://github.com/renium-rbx/renium/releases/latest).

| System | Install |
|---|---|
| Windows (x64, ARM64) | Run `Install-Renium.cmd`. It picks the right build, checks each download against the release's checksums and puts `rbx` on PATH. |
| macOS (Apple Silicon, Intel) | Extract the ZIP and open `Install Renium.command`. Capture and input may need Screen Recording or Accessibility permission. |
| Linux (x64, ARM64) | Extract the ZIP and run `./install.sh`. Studio does not run on Linux, so you get the offline tools, Open Cloud and plugins, but nothing that needs Studio. |

The installer sets up the Studio plugin and asks which editor, VS Code or Cursor,
should get the extension. Restart the editor and Studio afterward.

- `rbx upd` updates all three together.
- `renium` is another name for `rbx`.
- There is no server to start. The first `rbx` command starts Renium's background
  process, and the Studio plugin connects to it by itself.

## How sync works

- **Scripts** are plain `.luau` files under `src/`.
- **Everything else** in a service, from parts and UI to Terrain and the scripts' own
  properties, goes into a store at `instances/<Service>.renium`. A store is built
  from what Studio itself saves, so nothing is reduced to what a model file can
  describe.
- **Both directions.** Saved files go to Studio, and Studio edits come back as files.
- **Conflicts.** If both sides changed while Live Sync was off, Renium compares each
  with the last state they shared. Separate edits merge, and a real conflict waits
  for you to choose.
- **Git.** The stores are compressed binary, so Rojo, Argon and editors without
  Renium cannot read them. Run `rbx vci` once and Git shows and merges them as text.
- **Rojo.** A Rojo project converts with `rbx init`, and `rbx ir --preview` shows the
  result first.

## The native helper

A Studio plugin cannot set some saved data: the voxel grid Terrain is saved as,
`MeshPart.MeshId`, and the engine properties scripts are blocked from. Renium uses a
native helper for those. The helper:

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

### In a project

`rbx init` writes `RENIUM.md` and a `RENIUM/` folder of topic guides into the
project. It adds short pointers to them in `AGENTS.md` and `CLAUDE.md` without
removing what is already there.

An agent such as Claude Code or Codex reads `RENIUM.md` (about 1,200 tokens) and
opens a topic guide only when a task needs it.

### How an agent calls Renium

The agent runs `rbx` in its terminal, or calls it as one MCP tool, `rbx`, whose
input is the argument list. Either way it loads no tool schemas.

### What the installer sets up

- It adds a short note about `rbx` to the global instructions of Claude Code, Codex,
  Gemini CLI, OpenCode and Windsurf when it finds them.
- It registers `rbx mcp` as the `renium` MCP server in the configs of Claude Code,
  Codex, Cursor, Windsurf, Gemini CLI and OpenCode.
- It keeps a `.renium.bak` copy of each file it changes.

`rbx mcp --install` and `rbx mcp --uninstall` do the same by hand.

### Commands agents use most

Each command prints compact JSON, and `print` output comes back to the command
instead of Studio's Output window.

| Task | Command |
|---|---|
| Run Luau in Edit, or on the server during Play | `rbx l "return workspace.Gravity"` |
| Run Luau on play client 1 | `rbx lc "return game.Players.LocalPlayer.Name" 1` |
| Start Play with a server and two clients | `rbx play -s --players 2` |
| Wait until something is true in the game | `rbx wait "workspace:GetAttribute('Ready')" -t 30` |
| Read the server's errors | `rbx co --server --level error` |
| Click and press keys in a client | `rbx inp -p 1 click Shop.BuyButton wait 100 key E` |
| Screenshot a client window | `rbx sc --client -p 1 -o shot.png` |
| Find saved instances with Studio closed | `rbx f Workspace -n Door` |

`rbx` lists every command and `rbx COMMAND --help` shows its options.

## Help

- **Questions:** the [Discord server](https://discord.gg/wwTFHSSNn3), where every
  release is announced.
- **Bugs:** [GitHub issues](https://github.com/renium-rbx/renium/issues).
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
