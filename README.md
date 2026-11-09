# Renium

Renium keeps a Roblox Studio place and a folder on your computer in sync, in both
directions. The whole place goes into the folder, not only the scripts, so the map,
UI, lighting and settings get Git history like code. The same `rbx` command also
runs playtests, Luau, input and screenshots in Studio from a terminal, so an AI
coding agent can build and test a game without someone clicking for it.

## Install

Download the package for your system from
[Releases](https://github.com/Superwheat/renium/releases/latest).

On Windows (x64 or ARM64), run `Install-Renium.cmd`. It picks the right build, checks
each download against the release's checksums and puts `rbx` on PATH. On macOS
(Apple Silicon or Intel), extract the ZIP and open `Install Renium.command`; capture
and input may need Screen Recording or Accessibility permission. On Linux (x64 or
ARM64), extract the ZIP and run `./install.sh`. Studio does not run on Linux, so you
get the offline tools, Open Cloud and plugins, but nothing that needs Studio.

The installer sets up the Studio plugin and asks which editor, VS Code or Cursor,
should get the extension. Restart the editor and Studio afterward. Later, `rbx upd`
updates all three together. `renium` is another name for `rbx`.

## Get started

Open your place in Studio, then run this in an empty folder:

```powershell
rbx init
rbx pl
rbx lon
```

`rbx init` creates the project, `rbx pl` pulls the place into files and `rbx lon`
starts Live Sync. Open the folder in your editor and work on either side. If the
folder already holds a project, run only `rbx lon`, because a pull would replace
your files with Studio's copy. To start from a place file, run `rbx init` in its
folder, open the file with `rbx so Place.rbxl`, then pull. There is no server to
start: the first `rbx` command starts Renium's background process, and the Studio
plugin connects to it by itself.

## How sync works

Scripts become plain `.luau` files under `src/`, and everything else in a service,
from parts and UI to Terrain and the scripts' own properties, goes into a store at
`instances/<Service>.renium` built from what Studio itself saves, so nothing is
reduced to what a model file can describe. Saved files go to Studio and Studio edits come back
as files. If both sides changed while Live Sync was off, Renium compares each with
the last state they shared: separate edits merge, and a real conflict waits for you
to choose. The stores are compressed binary, so Rojo, Argon and editors without
Renium cannot read them; run `rbx vci` once and Git shows and merges them as text.
A Rojo project converts with `rbx init`, and `rbx ir --preview` shows the result
first.

## The native helper

A Studio plugin cannot set some saved data, such as Terrain voxels, `MeshPart.MeshId`
and the engine properties scripts are blocked from. Renium uses a native helper for
those: it syncs Terrain, reads and writes protected properties, publishes packages
without dialogs, and captures or sends input to play windows that are not in front.
Each protected read or write through the helper waits for your approval by default.

On Windows, Renium loads the helper into the running Studio. On macOS it patches
the installed Studio app instead. `rbx setup`, which the installer runs, and
`rbx upd` apply the patch, and opening Studio with `rbx so` checks it. The patch
renames the engine to `Contents/MacOS/RobloxStudio.bin` and installs Renium's
launcher as `Contents/MacOS/RobloxStudio` with an ad-hoc signature. The original
launcher and code signature are kept under `~/Library/Application Support/Renium`,
so `rbx setup --uninstall` restores them. A Studio update clears the patch until
Renium applies it again.

The helper depends on Studio's internals. After a Studio update Renium finds them
again by itself; when that fails, the affected command stops with an error until
Renium is updated.

## Working with an AI agent

`rbx init` writes `RENIUM.md` and a `RENIUM/` folder of topic guides into the
project, and adds short pointers to them in `AGENTS.md` and `CLAUDE.md` without
removing what is already there. An agent such as Claude Code or Codex reads
`RENIUM.md` (about 1,200 tokens), opens a topic guide only when a task needs it,
and runs `rbx` in its terminal. It needs no MCP server and loads no tool schemas.
The installer also adds a short note about `rbx` to the global instructions of
Claude Code, Codex, Gemini CLI, OpenCode and Windsurf when it finds them.

Each command prints compact JSON, and `print` output comes back to the command
instead of Studio's Output window. These are the ones agents use most:

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

Ask questions in the [Discord server](https://discord.gg/wwTFHSSNn3), where every
release is announced. Report bugs as
[GitHub issues](https://github.com/Superwheat/renium/issues);
`rbx report -m "what went wrong"` writes a diagnostics zip to attach. The
[CLI guide](tools/renium/README.md) covers the project layout, Live Sync, saved
data, playtests, publishing, troubleshooting and building from source, and the
[extension readme](tools/renium-vscode-extension/readme.md) covers the editor side.
Contributors should read [AGENTS.md](AGENTS.md).

Renium is licensed under the GNU Affero General Public License v3.0 with the
Commons Clause condition, which does not grant the right to sell the software. See
[LICENSE](LICENSE).
