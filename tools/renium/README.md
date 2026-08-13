# Renium

Renium is a two-way sync tool for Roblox Studio. It mirrors a place into a plain
file tree — scripts as `.luau` files, everything else in one compact binary
store per service — and keeps Studio and your editor in sync in both
directions, with high property fidelity subject to the documented limitations
below.

Licensed under [AGPL-3.0 with the Commons Clause](../../LICENSE): free for
everyone, including commercial game development — the license covers the tool
itself, not what you make with it. Forks are welcome and must stay open
source; selling the software (or paid hosting/support built on it) is not
permitted.

It has three parts:

- **`renium.exe`** — the CLI that does all the work (this document).
- **VS Code/Cursor extension** — panels, live sync, git tab, `.renium` viewer.
  See `tools/renium-vscode-extension/readme.md`.
- **Studio plugin** — the in-Studio bridge the CLI talks to over WebSocket.
  See `tools/plugin_ws_bridge/README.md`.

AI agents: read `AGENTS.md` at the repository root instead — it is the compact
command guide written for that use.

## How a synced project looks

```text
src/
  Workspace/
    __roblox_sync_settings.renium     one store per service: instances,
    SomeScript.server.luau            properties, attributes
  ServerScriptService/
    ...
sourcemap.json
```

Script sources live as normal `.luau` files you edit directly. The `.renium`
store holds the instance tree and every other property. Together they contain
the saved project state. Studio is updated from them, and Studio edits are
written back.

## Installation

Put these two files anywhere together, or put the CLI on `PATH`:

```text
rbx.cmd   renium.exe        (Windows; or bin\renium.exe)
rbx       renium            (macOS; chmod +x both)
```

`rbx` is the short launcher used in every example below. It finds the CLI in
this order: `RENIUM_CLI` env var → `PATH` → next to the launcher → `bin\` next
to the launcher → source-repo build folder. macOS support is implemented but
not yet live-verified. Input and capture use Quartz and need Accessibility and
Screen Recording permissions.

## Quick start

1. Install the Studio plugin and open your place. The plugin connects to a
   local bridge daemon on ports 8781–8782.

   ```powershell
   rbx setup
   ```

   `setup` installs `Renium.rbxm` into your Roblox Plugins folder — from
   a copy next to the exe if present, otherwise downloading the latest GitHub
   release (`--file <path>` and `--dir <plugins dir>` override both). On macOS,
   it also prepares `~/Applications/Renium Studio.app`. Open that app instead
   of the original Studio app so Renium can serialize protected properties
   directly to a local file without showing a file picker. The original Studio
   app is not changed. The VS Code extension provides the same setup through
   **Renium: Install Studio Plugin** in the command palette and status-bar menu.
2. Start the daemon and leave it running:

   ```powershell
   rbx bd
   ```

3. Export the place into files:

   ```powershell
   rbx x -r . -d snapshots --run-import
   ```

   This writes service snapshots and imports them into `src/`. From the
   extension, use **Renium: Pull Studio to Files**. Use **Renium: Push Files to
   Studio** for the opposite direction.

4. Check the connection any time:

   ```powershell
   rbx status
   ```

The daemon is found automatically by later commands (env vars
`RENIUM_DAEMON`, `RENIUM_DAEMON_HOST`/`RENIUM_DAEMON_CONTROL_PORT`,
`RENIUM_DAEMON_FILE`, then `%LOCALAPPDATA%\Renium\daemon.json`, then the
default local endpoint).

`rbx bd` is a standalone long-running process. It keeps ports 8781-8782 open
until it is stopped. `--editor-stdio` is reserved for the VS Code extension's
owned child process: it reads daemon requests from stdin and exits when its
owner closes stdin. Do not use `--editor-stdio` when starting Renium manually.

## Projects, adapters, and builds

`renium.project.jsonc` is optional. It controls the source directory, projected
tree, mounts, adapters, filters, and script naming without replacing the
full-fidelity `.renium` stores.

```jsonc
{
  "$schema": "https://raw.githubusercontent.com/Superwheat/renium/main/tools/renium/schemas/renium.project.schema.json",
  "schemaVersion": 1,
  "name": "my-place",
  "sourceRoot": "game"
}
```

Service folders directly under `sourceRoot` are mapped automatically. Use `tree`
only for sources that need a different target. The extension Pull and Push
commands honor `sourceRoot`; it does not have to be `src`.
Renium discovers the nearest project file, or you can pin one globally:

```powershell
rbx --project .\renium.project.jsonc build -o .\build\place.rbxl
rbx fmt-project --project .\renium.project.jsonc
rbx explain-path .\game\ReplicatedStorage\Config.luau --project .\renium.project.jsonc
rbx generate-sourcemap --project .\renium.project.jsonc --stdout --filter "**/*.luau"
```

Create a project without replacing files that already exist:

```powershell
rbx init .\my-place --with git,wally,selene,docs
rbx init .\empty-project --preview
```

Adapters explicitly map non-Luau files into Roblox instances. TXT maps to
`StringValue`, CSV maps to `LocalizationTable`, model JSON maps to an instance
subtree, and JSON/JSONC/TOML/YAML/MessagePack/Markdown generate deterministic
ModuleScripts. Markdown is converted to escaped Roblox RichText before it is
returned. Roblox models and nested Renium or Rojo projects can be mounted.
Generated or one-way formats must be marked as such in the project file. Mounts
can be `exclusive`, `overlay`, or `read-only`, and can be `optional`.

```powershell
rbx adapters validate
rbx adapters build
rbx adapters build --check
rbx adapters watch
rbx adapters syncback
rbx adapters syncback --preview
rbx import-rojo --project .\default.project.json --preview
rbx import-rojo --project .\default.project.json --apply
```

Two-way TXT, CSV, and model-JSON adapters update their canonical instances
during `adapters build` or `adapters watch`. Studio imports update their source
files after pulling from Studio.

For a controlled Studio-to-files import, preview the included and ignored
instances plus every planned file write, deletion, and adapter update first:

```powershell
rbx syncback --input .\snapshots --list
rbx syncback --input .\snapshots --dry-run
rbx syncback --input .\snapshots -y
rbx import-path .\Shared.luau --path-json '["ReplicatedStorage","Shared"]' --dry-run
```

Filters are ordered and last-match wins. They can match path glob, name, class,
tag, attribute, or property and can apply to either sync direction. Ignored
Studio instances and ignored script sources remain unchanged, including during
a structural reconcile.

Shared settings merge in this order: user, workspace, experience, place, then
the project file's `settings` object. Explicit editor settings and CLI flags
override the merged value.

```powershell
rbx config list --origins
rbx config get liveSync.changesThreshold
rbx config set liveSync.changesThreshold 10 --scope place
rbx config unset liveSync.changesThreshold --scope place
rbx config edit --scope user
rbx config export -o effective-renium-config.json
```

## Everyday commands

Play testing:

```powershell
rbx ps          # start Play
rbx ps 2        # start a multiplayer test with 2 clients
rbx px          # stop Play
rbx pl          # play, wait 3 seconds, stop
rbx status      # play/edit status
rbx clients     # list connected Studio instances (edit/server/clients)
```

Run Luau in Studio:

```powershell
rbx l "print('hello')"           # server context
rbx l "return game.PlaceId"      # expressions return values
rbx lc "warn('client hi')"       # client context during Play
rbx lc "print('p2')" Player2     # a specific client in a multiplayer test
rbx lf .\script.luau             # from a file
```

Control Studio's built-in device simulator through the plugin API:

```powershell
rbx device list
rbx device set "iPhone 16 Pro" --orientation portrait
rbx device set --scaling fit
rbx device set --resolution 1179x2556 --pixel-density 460
rbx device status
rbx shot --studio -o iphone-16-pro.png
rbx device stop
```

The command accepts catalog names or stable ids and requires no keyboard,
mouse, focus, coordinates, or ribbon interaction. Notched devices reproduce
Studio's actual safe-area behavior for `DeviceSafeInsets`,
`ClipToDeviceSafeArea`, and `SafeAreaCompatibility`. Daemon clients can call
the same surface with command `device` and the CLI arguments in `args`. While
emulation is active, `rbx shot` automatically targets the simulated Studio
viewport. Use `--studio` to force it or `--client` to force the latest Play
client.

In multiplayer tests every instance runs its own bridge; `--player <name|N>`
on `lx`/`co` targets one client (`Player2` and `2` both work). Without a
selector, client commands go to the most recently focused client.

Simulate real input on a client (the window can stay in the background —
no focus is taken):

```powershell
rbx ui -p 2                                 # list visible buttons/textboxes with paths + ids
rbx press "PlayerGui.Shop.BuyButton" -p 2   # press a GUI button by path
rbx click 450 323 -p 1                      # click at viewport coordinates
rbx key E -p 2                              # key press; --hold-ms to hold
rbx type "hello" --path "PlayerGui.Menu.SearchBox" --enter
rbx shot -o client.png -p 2                 # screenshot the client viewport (unfocused ok; minimized windows are briefly restored without focus, then re-minimized)
renium wait-until "workspace:GetAttribute('Ready') == true" -c -t 20
```

Move the character and interact with the 3D world:

```powershell
rbx goto "Workspace.Shop.Door" -p 2   # pathfind-walk there; --tp teleports; --pos "x,y,z" for coords
rbx press "Workspace.Button" --world  # click a part or model's on-screen position
```

Multiple games open at once: every bridge reports its place (`rbx clients` shows
`placeName`, `placeId`, and `gameId`), and when connected bridges span more than one place,
commands refuse with the list instead of guessing. Pin commands to one game with
the `RENIUM_PLACE` env var or the global `--place <name|id|gameId:placeId>`
flag. The pair form is exact; names can be exact or substring matches. With a
single game open no filter is needed.

Multiple Studio windows can also have the same place open. `rbx clients` exposes
their distinct `runtimeId` values. Renium chooses the most recently focused
matching runtime once per command and pins every bridge channel to it, so a
chunked or parallel operation cannot combine two windows.

For an optional allowlist, put `allowedPlaceIds` and/or `allowedGameIds` arrays
in `renium.config.json` in the command's working directory. Set
`RENIUM_CONFIG=<path>` to use an explicit file. The daemon reloads this file for
new bridge handshakes and rejects malformed JSON instead of silently disabling
the guard. `RENIUM_ALLOW_ANY_PLACE=1` is the explicit bypass.

Duplicate GUI names resolve with `[n]` ordinals (`"Shop[2].BuyButton"`) or by
element id (`press -i <id>`); ambiguous presses fail with a candidate list, and
if exactly one match is visible it is picked automatically. `press` auto-scrolls
ScrollingFrame containers to bring the target into view; elements scrolled out
of view are excluded from `ui` until then. Legacy ClickDetectors
don't respond to injected clicks (engine limitation: hover events react, but
MouseClick validation follows the real hardware cursor) — ProximityPrompts
(`goto` + `key E`) and UserInputService-driven interaction work. `press`/`click`
take `--hold <ms>` (default 30) for the down→up gap. Short aliases:
`pr`, `clk`, `ky`, `ty`, `sc`, `wait`, `go`. Windows works today; macOS support
is implemented via Quartz events (needs Accessibility + Screen Recording
permissions) but has not been live-verified yet.

Read Studio console output:

```powershell
rbx c           # latest message
rbx cl 10       # last 10 messages
rbx co --follow --level error
rbx test --mode play --players 2 --timeout 30 --fail-on-error
```

Push editor changes to Studio:

```powershell
rbx push -r . -d src --upsert                                      # everything
rbx push -r . -d src -p src\Workspace\__roblox_sync_settings.renium --upsert
rbx push -r . -d src -p src\ServerScriptService\Main.server.luau --verify
```

If Studio rejects a read-only property, Renium shows the rejected items before
using its offline fallback. **Apply anyway** serializes the complete live place,
patches only those rejected values, closes that exact Studio process, and
reopens the same local file. The original file and Studio process are left
untouched if serialization or validation fails. This path does not send
keyboard or mouse input.

The protected-property prompt applies automatically after its countdown unless
**Not now** is chosen. Automation can resolve the active prompt through the
daemon without interacting with Studio:

```powershell
rbx review apply
rbx review skip
rbx review apply --review-id review-123-1
```

Set or delete one live instance property without a full push:

```powershell
rbx prop -s Workspace -p '["ModelName"]' -n Archivable -v true
rbx del  -s Workspace -p '["ModelName"]'
```

Export Studio to files:

```powershell
rbx x -r . -d snapshots --no-run-import    # snapshots only
rbx x -r . -d snapshots --run-import       # snapshots + import into src/
rbx x -r . --src-dir game -d snapshots --run-import
```

Friendly structural commands edit the canonical store by stable id:

```powershell
rbx create Workspace --class Folder --name NewFolder
rbx rename Workspace --id editor:id "New name"
rbx move Workspace --id editor:id --parent-id editor:parent
```

## Lifecycle, diagnostics, and publishing

Install scripts place `renium` and `rbx` on the user PATH and install the Studio
plugin:

```powershell
.\install.ps1
.\install.ps1 -Uninstall
```

```sh
./install.sh
./install.sh --uninstall
```

The CLI can inspect, repair, or remove the Studio plugin and can update matched
release components from a signed manifest:

```powershell
rbx setup --status
rbx setup --repair
rbx setup --uninstall
rbx update check
rbx update apply --component all --dry-run
rbx doctor --json
rbx doctor --bundle .\.renium\diagnostics\release-check
```

The editor extension checks the signed GitHub Release manifest when it opens.
The daemon also checks after the first connection from each Studio process, then
ignores reconnects from that process for five minutes. The update prompt installs
the matching extension and Studio plugin together. The
Studio plugin shows the same update in its notification card and sends the
install request to the connected editor. Reload the editor and restart Studio
after installation.

Named daemons are selected with the global `--daemon` flag:

```powershell
rbx --daemon playtest bd
rbx daemon list
rbx daemon status playtest
rbx daemon stop playtest
rbx daemon clean
```

Resolve or launch the exact Studio file without choosing an ambiguous project:

```powershell
rbx studio .\place.rbxl --check
rbx --project .\renium.project.jsonc studio
```

Publish a place through Open Cloud. Renium validates the universe and place
against `renium.experience.json` when that file exists:

```powershell
$env:ROBLOX_API_KEY = "..."
rbx --project .\renium.project.jsonc upload-place --universe-id 123 --place-id 456
```

Agents can use the bound automation context for any JSON Open Cloud endpoint.
The daemon reuses one HTTPS client, fills `{universe}` and `{place}` from the
context, and accepts several requests in one payload:

```json
{
  "requests": [{
    "method": "GET",
    "path": "/cloud/v2/universes/{universe}/data-stores",
    "query": { "maxPageSize": 25 }
  }]
}
```

```powershell
rbx a cloud CX -J open-cloud.json
```

`ROBLOX_API_KEY` must be set before the daemon starts. See the generated
`AGENTS.md` for batched Creator Store and data-store recipes. API keys aren't
accepted in payloads or command arguments.

The automation API also covers the plugin-accessible Creator features used by
Roblox's Studio MCP: Creator Store and user-inventory search, asset insertion,
AI model generation jobs, local image validation, and Open Cloud image upload.

```powershell
rbx a asset-search CX -J asset-search.json
rbx a asset-insert CX -J asset-insert.json
rbx a generate-model CX -J generate-model.json
rbx a job-status CX -J job-status.json
rbx a image-store CX -J image-store.json
rbx a image-upload CX -J image-upload.json
```

`generate-model` uses the public `GenerationService:GenerateModelAsync` plugin
API. Material generation and Roblox's internal primitive `ProceduralModel`
generator require `RobloxScriptSecurity`, so third-party plugins cannot expose
them reliably. Group and universe Creator Inventory searches are also absent
from public plugin and Open Cloud APIs; Renium reports them as unsupported.
HTTP image uploads use the locally installed plugin's prompt-free
`AssetService:CreateAssetAsync` API. Local files and explicit user/group
ownership use Open Cloud instead.

Ordered mouse and keyboard sequences use `rbx a input CX -J input.json`.
Windows posts events to the exact target window handle and macOS posts Quartz
events to the exact target process. Neither implementation moves the system
cursor, sends global input, or activates another application.

Agents can record the edit viewport or one play client without activating its
window or capturing the rest of the desktop:

```powershell
rbx a record-start CX -J record-start.json
rbx a record-end CX -J record-end.json
```

`record-start` accepts `player`, `studio`, `client`, an `output` path ending in
`.webp`, `fps` from 1 through 30, `maxSeconds` from 1 through 300, and `quality`
from 0 through 100. Pass its returned `recordingId` to `record-end`. The result
is an animated WebP with no audio and can be attached directly as a clip.

`rbx docs [topic]` prints the bundled reference. `rbx docs --serve` exposes the
same text on a read-only loopback page.

## Exploring and editing the store

Three high-level read commands work directly on `src/<Service>/…renium` files —
no Studio connection needed:

```powershell
rbx find Workspace VipMan              # locate instances by name
rbx find Workspace --class Script      # ...or class, tags, properties
rbx tree Workspace VipMan --depth 2 --limit 100    # browse children
rbx inspect Workspace VipMan           # one instance in detail
```

Output is compact JSON built for scripting and agents. When a bare name is
ambiguous, the command returns the candidates (with `id`, `path`, `ords`)
instead of guessing; retry with `--id`, a fuller `--path`, or `--ords`.

Edit the store directly with the `b*` (bytecode) commands, then `rbx push`:

```powershell
rbx bs  Workspace -i editor:id -p DisplayName --str "VIP Man"   # set property
rbx bss Workspace -i editor:script-id --source-file big.luau    # set source
rbx ba  Workspace -n NewModel -c Model                          # add
rbx bcl Workspace -i editor:source-id -I editor:parent-id       # clone
rbx br  Workspace -i editor:id                                  # remove
rbx bg  Workspace -i editor:script-id -p Source                 # get property
```

Selector rules that prevent surprises:

- Select with `-i` (settings id), `-x` (index), `-n`/`-c` (name/class), or
  `--path` + optional `--ords`. Don't combine `--path` with the others.
- A bare service name resolves `src\<Service>\__roblox_sync_settings.renium`;
  pass an explicit file with `-f` instead — never both.
- Duplicate names or paths are rejected rather than silently picking the first
  match; disambiguate with `--ords`, `--id`, or `--index`.
- `--scope auto|metadata|property|attribute` controls what a property write
  targets; `auto` is right almost always.

Batched low-level reads go through `rbx bb` (one call, many queries). The op
types are `counts`, `service`, `search`, `children`, `instance`, and `find`;
see `AGENTS.md` for recipes and field presets.

Export/import whole models and places:

```powershell
rbx bem Workspace -i editor:id -o model.rbxm    # store subtree -> .rbxm
rbx bim model.rbxm --service Workspace          # .rbxm -> store
rbx bep -o place.rbxl                           # whole place file
```

## Links (shared code across places)

A renium-link mirrors one source into one or more targets. Sources can be a
local file/folder, a git repo (branch/tag/commit + subpath, private repos use
your git credentials), or an installed Wally package. Mirrors are read-only by
default: editing a mirror reverts on the next apply, editing the source fans
out to every mirror. `--writable` links instead keep local edits and report
them as `preservedEdits`.

```powershell
# Control links/Logger.luau from two places in the tree.
rbx lka --source-type local --source links/Logger.luau --service ReplicatedStorage --path '["ReplicatedStorage","Modules","Logger"]'
rbx lka --id logger --source-type local --source links/Logger.luau --service ServerScriptService --path '["ServerScriptService","Logger"]'

# Pin a git source.
rbx lka --id uikit --source-type git --source https://github.com/org/ui-kit --ref v1.2.0 --subpath src --service ReplicatedStorage --path '["ReplicatedStorage","UIKit"]'

rbx lk              # materialize all targets (then `rbx push`)
rbx lk --check      # report drift, write nothing
rbx lk --strict     # CI mode: warnings fail with exit 1
rbx lks             # status: targets, drift, broken, resolved refs

# Detach one target (becomes editable) or a whole link.
rbx lkb --service ServerScriptService --path '["ServerScriptService","Logger"]'
rbx lkb --link logger
```

The manifest is `renium-link.json` at the project root. Git/Wally clones are
cached in `.renium/link-cache` (auto-ignored); the lockfile
`.renium/link.lock.json` commits so pinned sources reproduce. Override the
cache location with `--cache-dir`, the manifest `cacheDir` field, or the
`renium.link.cacheDir` VS Code setting.

Treat third-party `.renium` packages like source-code dependencies, not inert
data: they can contain scripts that run, arbitrary properties, and
`PackageLink` instances. `rbx lkp` packs an existing subtree into a reusable
package; `rbx link-delete-package` removes one.

In VS Code/Cursor, linked files show an `L` badge and open read-only;
right-click for **Break Link** / **Reveal Link Source**.

## Version control

A synced project is a normal git repo — scripts are plain text and each
service has one binary store. Run once per repo:

```powershell
rbx vc-init                                            # in the project root
rbx vc-init --remote https://github.com/you/your-game  # also set origin
rbx vc-init --skip-git                                 # only write policy files
```

It is idempotent and sets up: `.gitattributes` (LF policy, marks `*.renium`
binary with a diff/merge driver), `.gitignore` (build artifacts, snapshots,
locks), `.renium/.gitignore` (cache local, lockfile committed), and the git
config for the textconv + merge driver.

After that:

- `git diff` on a `.renium` store renders as text: one line per instance plus
  sorted property lines. Script bodies show as line-count + fingerprint since
  they already diff as `.luau` files.
- `git merge` of divergent stores merges at the instance/property level.
  Parallel edits to different properties merge cleanly; same-property edits
  conflict with an exact report (path + property + both values). Resolve with
  `git checkout --ours/--theirs -- <file>` or standalone:
  `rbx vc-merge <base> <ours> <theirs> --prefer theirs -o <file>`.

Typical flow:

```text
edit in Studio / editor  ->  renium syncs src/         ->  git commit + push
git pull                 ->  rbx lk (--strict in CI)   ->  rbx push to Studio
```

Studio and the daemon never see git; version control is purely over the file
tree, like Rojo — but with full property fidelity and mergeable stores.

## Inspecting a .renium file

```powershell
rbx view src\SoundService\__roblox_sync_settings.renium   # text tree
rbx view links\ui-kit.renium --json                       # structured JSON
```

Same decoder as the sync itself, so what you see is what syncs. In VS Code,
double-click any `.renium` for the same view.

## Wally packages

`rbx wally` runs `wally install` and imports the package tree straight into
the stores — Rojo is not needed. It is `wally.lock`-aware (unchanged installs
are a no-op) and maps realms:

```text
shared -> Packages       -> ReplicatedStorage/Packages
server -> ServerPackages -> ServerStorage/ServerPackages
dev    -> DevPackages    -> ReplicatedStorage/DevPackages
```

```powershell
rbx wally                   # install + import every present realm
rbx wally --realms shared
rbx wally --force           # re-import even if wally.lock is unchanged
rbx wally --skip-install
```

## PowerShell 5.1 quoting

Inline-JSON examples here are written for PowerShell 7+. Windows
PowerShell 5.1 strips embedded double quotes when calling native executables,
so `-j '[{"type":"counts"}]'` arrives mangled. On 5.1 either escape:

```powershell
rbx bb Workspace -j '[{\"type\":\"counts\"}]'
```

or use an ops file:

```powershell
'{"ops":[{"type":"counts"}]}' | Set-Content ops.json -Encoding ascii
rbx bb Workspace -J ops.json
```

## Command aliases

```text
bd   bridge-daemon            bg   bytecode-get-property
x    export-snapshots         bs   bytecode-set-property
ed   explorer-daemon          bss  bytecode-set-source
src  bridge-get-source        bb   bytecode-explorer-batch
co   get-console-output       bt   bytecode-editor-targets
lx   execute-luau             ba   bytecode-add-instance
dev  studio-device
play start-stop-play          bcl  bytecode-clone-instance
st   studio-change-state      br   bytecode-remove-instance
push push-editor-changes      bep  bytecode-export-place
review editor-review-decision
prop apply-editor-property    bim  bytecode-import-model
del  apply-editor-delete      bpack bytecode-repack
rev  editor-revert            wally sync-wally-packages
im   import-snapshots         lk   link-apply
ims  import-service           lkb  link-break
sm   generate-sourcemap       lks  link-status
vci  vc-init                  lka  link-add
vct  vc-textconv              lkp  link-pack
vcm  vc-merge                 v    view
```

Plus the `rbx.cmd`-level shortcuts: `rbx l/lf/lc/lcf` (Luau), `rbx c/cl`
(console), `rbx ps/px/pl` (play), `rbx status`. Anything else passes through
to `renium.exe` unchanged.

## Building from source

Released binaries don't require a build. From the source repo:

```powershell
cargo build --locked --release --manifest-path tools\renium\Cargo.toml
```

If the exe is locked by a running daemon, stop it first:

```powershell
Get-Process renium -ErrorAction SilentlyContinue | Stop-Process -Force
```

Full release builds (CLI + extension VSIX + both plugin bundle formats, with
version cross-checks, hashes, and a manifest under `dist/`):

```powershell
.\tools\build-release.ps1 -LocalBuild
```

Omit `-LocalBuild` only for a public release; that mode intentionally requires
a clean checkout, a root `LICENSE` file, and a registered VS Code publisher.

## Good to know

- Prefer settings ids over names in scripts and automation — names can repeat.
- For model pivots the property is `WorldPivot`.
- `rbx bd` stays alive while Studio is closed or reconnecting. It exits only
  when stopped. `--editor-stdio` is for the VS Code extension's owned child
  process and exits when the editor closes its stdin stream.
- Live sync = editor changes push to Studio, and dirty Studio services import
  back after serve/plugin connection. Dirty detection uses per-instance
  property/attribute listeners, service-root descendant add/remove listeners,
  and CollectionService tag membership signals. Newly created tag names are
  discovered during the session. Those listeners stay active once live sync
  has started, which is what catches edits made while the editor was
  disconnected.
- Studio imports use stable `GetDebugId` ids, so reparenting an instance in
  Studio updates its parent in the store instead of duplicating it.
- Filesystem-to-Studio mutations create a Studio undo recording and preserve
  Explorer selection across class/full-import replacements. A failed batch
  stops at its first error so the recorded partial work can be undone as one
  Renium action.
- Snapshot imports move stale generated paths to
  `.renium/import-backups/<timestamp>/` instead of permanently deleting them.
  The manual VS Code import command asks for confirmation; automated live sync
  remains non-interactive.
- Script comparison ignores CRLF/LF-only differences while retaining the
  existing filesystem line-ending convention.

## Known limitations

- Two writable properties can't be carried: `TextChatMessage.Timestamp`
  (`DateTime` isn't serializable by rbx-dom, so place files never contain it)
  and the Studio-only `QDir`/`QFont` settings fields. `Axes`, `Faces`, and
  `Ray` properties sync fully.
- Modern `Content` properties preserve URI and `None` sources. `Object` and
  `Opaque` sources can't be represented by the current file store, so export
  stops with an error instead of replacing them with empty content.
- Infinity, negative Infinity, and NaN use Renium's tagged float transport and
  round-trip without being converted to finite JSON numbers.

## Automation opcode registry

<!-- automation-opcodes:start -->
Protocol version: `1`

| ID | Operation | Aliases | Review |
|---:|---|---|:---:|
| 0 | `cap` | - | no |
| 1 | `bind` | - | no |
| 2 | `context` | - | no |
| 3 | `unbind` | - | no |
| 10 | `pull` | - | yes |
| 11 | `push` | - | yes |
| 12 | `live-start` | - | no |
| 13 | `live-stop` | - | no |
| 14 | `live-status` | - | no |
| 15 | `retry-pending` | - | no |
| 16 | `discard-pending` | - | no |
| 20 | `find` | - | no |
| 21 | `tree` | - | no |
| 22 | `inspect` | - | no |
| 23 | `batch` | bb | no |
| 24 | `script-search` | - | no |
| 25 | `script-read` | - | no |
| 26 | `script-grep` | - | no |
| 30 | `get-property` | - | no |
| 31 | `set-property` | - | yes |
| 32 | `set-source` | - | no |
| 33 | `add` | - | no |
| 34 | `clone` | - | no |
| 35 | `move` | - | no |
| 36 | `remove` | - | no |
| 37 | `revert` | - | no |
| 38 | `multi-edit` | - | no |
| 40 | `import-model` | - | no |
| 41 | `export-model` | - | no |
| 42 | `export-place` | - | no |
| 43 | `import-snapshots` | - | no |
| 44 | `export-snapshots` | - | no |
| 45 | `sourcemap` | - | no |
| 50 | `studios` | - | no |
| 51 | `studio-status` | - | no |
| 52 | `studio-open` | - | yes |
| 53 | `studio-close` | - | yes |
| 54 | `luau` | - | no |
| 55 | `console` | - | no |
| 56 | `play-start` | - | no |
| 57 | `play-stop` | - | no |
| 58 | `shot` | - | no |
| 59 | `device` | - | no |
| 60 | `ui` | - | no |
| 61 | `press` | - | no |
| 62 | `click` | - | no |
| 63 | `key` | - | no |
| 64 | `type` | - | no |
| 65 | `wait` | - | no |
| 66 | `goto` | - | no |
| 67 | `input` | - | no |
| 68 | `record-start` | - | no |
| 69 | `record-end` | - | no |
| 70 | `project-init` | - | no |
| 71 | `project-validate` | - | no |
| 72 | `place-add` | - | no |
| 73 | `place-rename` | - | no |
| 74 | `place-reorder` | - | no |
| 80 | `review-prepare` | - | no |
| 81 | `review-apply` | - | no |
| 82 | `review-reject` | - | no |
| 90 | `cloud` | oc | no |
| 91 | `asset-search` | - | no |
| 92 | `asset-insert` | - | no |
| 93 | `generate-model` | - | no |
| 94 | `job-status` | - | no |
| 95 | `image-upload` | - | no |
| 96 | `image-store` | - | no |
| 97 | `http-get` | - | no |
<!-- automation-opcodes:end -->
