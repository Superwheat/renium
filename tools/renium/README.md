# Renium

Renium syncs Roblox Studio with a file tree in both directions. Scripts use
`.luau`; other instances and properties use one compact store per service.

Licensed under [AGPL-3.0 with the Commons Clause](../../LICENSE). Commercial
game development is allowed; selling Renium or paid hosting/support is not.
Forks must remain open source.

It has three parts:

- **`renium.exe`** — CLI and daemon.
- **VS Code/Cursor extension** — sync controls, Git, and `.renium` viewer.
  See `tools/renium-vscode-extension/readme.md`.
- **Studio plugin** — Studio bridge.
  See `tools/plugin_ws_bridge/README.md`.

Agents should read the repository's `AGENTS.md` instead.

## How a synced project looks

```text
src/
  Workspace/
    __roblox_sync_settings.renium     one store per service: instances,
    SomeScript.server.luau            properties, attributes
  ServerScriptService/
    ...
sourcemap.json
renium.project.jsonc
```

Edit `.luau` files directly. `.renium` stores hold the remaining project state.

## Installation

Keep the launcher and CLI together, or put the CLI on `PATH`:

```text
rbx.exe   renium.exe        (Windows; `rbx.cmd` remains a fallback)
rbx       renium            (macOS and Linux; chmod +x both)
```

Examples use `rbx`. It checks `RENIUM_CLI`, its own directory, `bin/`, the
installed Renium directory, then `PATH`. macOS input and capture need
Accessibility and Screen Recording permissions and aren't yet live-verified.

## Quick start

1. Install the Studio plugin and open your place.

   ```powershell
   rbx setup-renium
   ```

   This uses a nearby plugin or downloads the latest release. Override it with
   `--file` or `--dir`. On macOS, use the generated
   `~/Applications/Renium Studio.app` for protected-property support. The
   extension also provides **Renium: Install Studio Plugin**.
2. Pull the place into files:

   ```powershell
   rbx pull
   ```

   This writes snapshots under `.renium` and imports the place into `src/`.
   The extension has matching Pull and Push commands.

3. Check the connection any time:

   ```powershell
   rbx studio-status
   ```

Commands find the daemon through env vars
`RENIUM_DAEMON`, `RENIUM_DAEMON_HOST`/`RENIUM_DAEMON_CONTROL_PORT`,
`RENIUM_DAEMON_FILE`, then `%LOCALAPPDATA%\Renium\daemon.json`, then the
default local endpoint.

`rbx bridge-daemon` runs a standalone daemon. Other commands start or reuse one.
`--editor-stdio` is reserved for the extension and exits when its stdin closes.

Agents use short names; descriptive aliases call the same commands:

| Short | Descriptive alias |
|---|---|
| `pl` / `ps` | `pull` / `push` |
| `lon` / `lof` / `lst` | `live-start` / `live-stop` / `live-status` |
| `f` / `tr` / `in` | `find` / `tree` / `inspect` |
| `q` / `cmp` | `query-place` / `compare-place` |
| `l` / `lc` / `co` | `luau` / `execute-client-luau` / `console` |
| `sc` / `rs` / `re` | `screenshot` / `record-start` / `record-end` |
| `pr` / `clk` / `ky` / `ty` / `go` | `press` / `click` / `key` / `type` / `goto` |
| `dev` / `cs` | `device` / `clients` |
| `pf` | `performance-profile` |
| `oc` | `cloud` |

`rbx` lists short names. There is no separate automation layer.

## Projects, adapters, and builds

Each place has `renium.project.jsonc`. Add fields only for custom source roots,
trees, mounts, adapters, filters, or script naming. Project data remains in
`.renium` stores.

```jsonc
{
  "schemaVersion": 1,
  "sourceRoot": "game"
}
```

Service folders under `sourceRoot` map automatically. Use `tree` for exceptions.
The extension honors `sourceRoot`. Renium finds the nearest project or accepts
`--project`:

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

Without `--with`, initialization creates the project marker, `src`, Renium
guides, and marked pointers in `AGENTS.md` and `CLAUDE.md`. Existing guidance is
preserved. Preview writes nothing. Invalid required paths stop initialization.

Adapters map non-Luau files to instances: TXT → `StringValue`, CSV →
`LocalizationTable`, model JSON → subtree, and structured text/MessagePack/
Markdown → deterministic ModuleScripts. Models and nested Renium/Rojo projects
can be mounted as `exclusive`, `overlay`, `read-only`, or `optional`.

```jsonc
{
  "mounts": [
    {
      "source": "shared",
      "target": "ReplicatedStorage.Shared",
      "ownership": "read-only",
      "optional": true
    }
  ]
}
```

`exclusive` is the default. Missing optional sources project nothing. Reads
include mounts; `bss` edits writable mounted scripts; `explain-path` follows
nested projects.

```powershell
rbx project-validate
rbx adapters validate
rbx adapters build
rbx adapters build --check
rbx adapters watch
rbx adapters syncback
rbx adapters syncback --preview
rbx import-rojo --project .\default.project.json --preview
rbx import-rojo --project .\default.project.json --apply
```

`project-validate` checks the project offline and exits nonzero on errors.

Two-way TXT, CSV, and model-JSON adapters update instances during build/watch
and source files after a pull.

Preview controlled Studio → files imports first:

```powershell
rbx syncback --input .\snapshots --list
rbx syncback --input .\snapshots --dry-run
rbx syncback --input .\snapshots -y
rbx import-path .\Shared.luau --path-json '["ReplicatedStorage","Shared"]' --dry-run
rbx import-path .\Shared.luau --path-json '["ReplicatedStorage","Shared"]'
rbx import-path .\SharedFolder --destination src\ReplicatedStorage\Shared --dry-run
```

`--path-json` maps one file to a Roblox path and keeps script suffix semantics.
`--destination` uses a project-relative path. Results show `create`,
`overwrite`, or `unchanged`; omit `--dry-run` to apply. `--push` also updates
Studio and requires destinations in the active project.

Filters match glob, name, class, tag, attribute, property, or ID. Last match
wins. Ignored instances and fields remain unchanged in either direction.

`syncRules` map other files. Last match wins; `suffix` is removed from the
instance name, `exclude` rejects a match, and `use: "ignore"` suppresses a file.
Script uses are `moduleScript`, `serverScript`, `clientScript`, and
`pluginScript`; adapter formats also work. `globIgnorePaths` blocks paths before
projection.

```jsonc
{
  "syncRules": [
    {
      "pattern": "**/*.server.txt",
      "use": "serverScript",
      "suffix": ".server.txt"
    },
    {
      "pattern": "**/draft/**",
      "use": "ignore"
    }
  ],
  "globIgnorePaths": ["src/generated/**"],
  "filters": [
    {
      "action": "ignore",
      "direction": "files-to-studio",
      "class": "ModuleScript"
    },
    {
      "action": "include",
      "direction": "files-to-studio",
      "name": "Shared"
    },
    {
      "action": "ignore",
      "direction": "both",
      "glob": "Workspace/Generated/**",
      "property": "Source"
    }
  ]
}
```

Actions are `include` or `ignore`; directions are `files-to-studio`,
`studio-to-files`, or `both`. Field selectors affect only that field.
`explain-path` reports ownership, ignored paths, winning rules, filters, and
both-direction decisions.

### Layered configuration

Settings merge in this order: user, workspace, experience, place, project.
Editor settings and CLI flags override them.

`list` shows every setting, its current value, and valid values. `get` reads one
value; `list --origins` also shows its source. Writes use `--scope`. `export`
writes the merged configuration.

```powershell
rbx cfg list
rbx cfg get liveSync.changesThreshold
rbx cfg set liveSync.changesThreshold 10
rbx cfg unset liveSync.changesThreshold
rbx cfg list --origins
rbx cfg reset --scope place
rbx cfg path --scope workspace
rbx cfg edit --scope user
rbx cfg export -o effective-renium-config.json
```

## Everyday commands

Play testing:

```powershell
rbx play -s                         # ordinary Play
rbx play -s --players 2             # local server with 2 clients
rbx play -x                         # stop Play
rbx studio-status                   # play/edit status
rbx clients     # list connected Studio instances (edit/server/clients)
```

Run Luau in Studio:

```powershell
rbx luau "print('hello')"                         # server context
rbx luau "return game.PlaceId"                    # expressions return values
rbx execute-client-luau "warn('client hi')"       # client context during Play
rbx execute-client-luau "print('p2')" Player2     # one multiplayer client
rbx luau --file .\script.luau                     # from a file
```

Control Studio's built-in device simulator through the plugin API:

```powershell
rbx device list
rbx device set "iPhone 16 Pro" --orientation portrait
rbx device set --scaling fit
rbx device set --resolution 1179x2556 --pixel-density 460
rbx device status
rbx screenshot --studio -o iphone-16-pro.png
rbx device stop
```

`device set` returns the new state. Add `--details` for native dimensions and
density.

Devices accept names or stable IDs. Notched presets reproduce Studio safe-area
behavior. With emulation active, screenshots target the simulated viewport;
`--studio` or `--client` overrides the target.

Constrain Studio resources independently of device simulation and FPS:

```powershell
rbx pf ls
rbx pf use iphone-11
rbx pf show
rbx pf off
rbx pf adv cpu=25 cores=2 headroom=1g prio=low
rbx pf adv cpu=40 cores=4 headroom=2g save=slow-test
```

Built-in device names are approximate performance tiers, not hardware
emulation. Renium calibrates the computer and only offers tiers it can enforce
without exceeding native performance. The selected profile applies globally to
connected Studio process trees and follows replacement Studio processes.

Advanced profiles accept aggregate CPU percent, logical cores, memory
headroom, and `normal`, `below`, or `low` priority. Absolute memory caps use
`mem=` and require `risk=crash` because they can terminate Studio. Windows
enforces and reads back the limits with Job Objects. macOS and Linux report the
feature as unavailable when equivalent reversible controls don't exist.

In multiplayer, `--player <name|N>` targets one client. Without it, client
commands use the latest focused client.

Send input without focusing the client:

```powershell
rbx ui -p 2                                 # list visible buttons/textboxes with paths + ids
rbx press "PlayerGui.Shop.BuyButton" -p 2   # press a GUI button by path
rbx click 450 323 -p 1                      # click at viewport coordinates
rbx key E -p 2                              # key press; --hold-ms to hold
rbx type "hello" --path "PlayerGui.Menu.SearchBox" --enter
rbx screenshot -o client.png -p 2           # capture the client viewport
renium wait-until "workspace:GetAttribute('Ready') == true" -c -t 20
```

Move the character and interact with the 3D world:

```powershell
rbx goto "Workspace.Shop.Door" -p 2   # pathfind-walk there; --tp teleports; --pos "x,y,z" for coords
rbx press "Workspace.Button" --world  # click a part or model's on-screen position
```

With several games open, ambiguous commands list candidates. Pin one with
`RENIUM_PLACE` or `--place <name|id|gameId:placeId>`. Names allow substrings;
the ID pair is exact.

For duplicate Studio windows, `rbx clients` shows each `runtimeId`. Renium pins
one runtime per command.

Optional allowlists use `allowedPlaceIds` or `allowedGameIds` in
`renium.config.json`; select another file with `RENIUM_CONFIG`. Invalid JSON is
rejected. `RENIUM_ALLOW_ANY_PLACE=1` bypasses the list.

Resolve duplicate UI names with `[n]` or `-i <id>`. Ambiguity returns
candidates; one visible match is selected. `press` scrolls targets into view.
Injected clicks can't fire `ClickDetector`; use ProximityPrompts or game input.
`press`/`click --hold <ms>` controls the down/up delay. Windows and macOS target
the selected window; Linux uses the Play client's virtual-input API. The orange
shield blocks interfering physical input.

Read Studio console output:

```powershell
rbx console -n 1
rbx console -n 10
rbx console --follow --level error
rbx test --mode play --players 2 --timeout 30 --fail-on-error
```

Push editor changes to Studio:

```powershell
rbx push -r . -d src --upsert                                      # everything
rbx push -r . -d src -p src\Workspace\__roblox_sync_settings.renium --upsert
rbx push -r . -d src -p src\ServerScriptService\Main.server.luau --verify
```

For rejected read-only properties, **Apply anyway** serializes the place,
patches only rejected values, then reopens the same Studio target. Failure
leaves the original file and process untouched.

The protected-property prompt applies after its countdown unless skipped.
Automation may resolve it directly:

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
rbx pull                                      # Studio -> project files
rbx export-snapshots -r . -d snapshots --no-run-import    # snapshots only
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
rbx setup-renium --status
rbx setup-renium --repair
rbx setup-renium --uninstall
rbx update
rbx update check
rbx update apply --component all --dry-run
rbx doctor --json
rbx doctor --bundle .\.renium\diagnostics\release-check
```

`doctor` checks the project, configuration, optional tools, plugin, and daemon.
Warnings exit 0; errors exit 1. `--json` returns checks and build identity.
`--bundle` writes diagnostics and the loaded project file.

The extension checks signed releases on startup and caches results for five
minutes. Updating installs matching extension and plugin versions. If the
plugin changes, choose whether local places stay open, save and close, or close
without saving; this choice can be remembered. Closed targets reopen after the
update without Studio's save dialog.

Named daemons are selected with the global `--daemon` flag:

```powershell
rbx --daemon playtest bridge-daemon
rbx daemon list
rbx daemon status playtest
rbx daemon stop playtest
rbx daemon clean
```

Open an exact Studio target:

```powershell
rbx studio .\place.rbxl --check
rbx --project .\renium.project.jsonc studio
```

Publish through Open Cloud. `renium.experience.json` validates the IDs when
present:

```powershell
$env:ROBLOX_API_KEY = "..."
rbx --project .\renium.project.jsonc upload-place --universe-id 123 --place-id 456
```

OAuth tokens may come from a named environment variable:

```powershell
rbx upload-place --oauth-env ROBLOX_OAUTH_TOKEN
```

Open Cloud runs without Studio or a daemon. The project supplies universe and
place IDs when available:

```powershell
rbx cloud key
rbx cloud data stores --limit 25
rbx cloud data get PlayerData user-42
rbx cloud universe message updates refresh
rbx cloud user inventory 42 --limit 25
rbx cloud product create "Refresh Daily Rewards" --price 27 --for-sale --regional-pricing
rbx cloud place publish place.rbxl
rbx cloud analytics metrics --field metric=DailyActiveUsers --field granularity=OneDay --field startTime=2026-01-01T00:00:00Z --field endTime=2026-02-01T00:00:00Z
rbx cloud event list --limit 10
rbx cloud experiment list --limit 25
rbx cloud thumbnail upload first.png --file files=second.png
```

Set `ROBLOX_API_KEY`, or use `--oauth-env`/`--key-env`. Credentials aren't
accepted in arguments or payloads. `cloud key` reports scopes and targets
without the secret. `--anonymous` is explicit and never used as a fallback.
Native commands cover Roblox data, memory, universes, places, users, groups,
assets, commerce, localization, servers, analytics, events, AI, and related
APIs. `cloud routes [CATEGORY]` lists operations; generic requests cover new
endpoints.

Creator features include search, asset insertion, model generation, image
validation, and Open Cloud image upload.

Common reads and Studio creator jobs have direct commands:

```powershell
rbx asset-search "wooden crate" --limit 5
rbx asset-insert 182451181 --parent Workspace --name AuditCrate
rbx generate-model "small wooden crate" --parent Workspace --name GeneratedCrate --size 4,4,4 --max-triangles 2000
rbx job-status JOB_ID --wait-seconds 30
rbx image-store assets/reference.png
rbx image-upload assets/reference.png --user USER_ID --name Reference --open-cloud
```

Insertion and generation edit the live runtime; pull or save to persist them.
Generation returns a job ID and status. `image-store` validates PNG, JPEG, BMP,
or TGA files up to 5 MiB without uploading. Use normal web tools for Roblox
documentation.

Image upload requires an explicit user or group owner. Unpublished projects may
need `--universe`, `--place-id`, or `--param`.

Material generation, internal procedural models, and group/universe Creator
Inventory search aren't available to third-party plugins or Open Cloud and are
reported as unsupported.

`input` reads action/value pairs left to right, for example
`rbx input --player 1 click "Shop.BuyButton" wait 100 key E`. Input targets one
window without moving the cursor or taking focus. The shield follows it.

Record one Studio or client viewport:

```powershell
rbx record-start -p 2 -o test.mp4
rbx key W --hold-ms 700 -p 2
rbx record-end
```

`record-start` accepts a target, `.mp4` path, 1–30 FPS, 1–300 seconds, and
quality 0–100. `record-end` stops the active recording; an optional ID verifies
it. Output is silent H.264 MP4.

`rbx docs [topic]` prints bundled docs; `--serve` opens a read-only local page.

## Exploring and editing the store

Read stores without Studio:

```powershell
rbx find Workspace VipMan              # locate instances by name
rbx find Workspace --class Script      # ...or class, tags, properties
rbx tree Workspace VipMan --depth 2 --limit 100    # browse children
rbx inspect Workspace VipMan           # one instance in detail
```

Ambiguity returns candidates with IDs, paths, and ordinals; refine the selector.

Edit stores, then push:

```powershell
rbx set-property Workspace -i editor:id -p DisplayName --str "VIP Man"
rbx set-source Workspace -i editor:script-id --source-file big.luau
rbx add Workspace -n NewModel -c Model
rbx bytecode-clone-instance Workspace -i editor:source-id -I editor:parent-id
rbx bytecode-remove-instance Workspace -i editor:id
rbx get-property Workspace -i editor:script-id -p Source
```

Selector rules:

- Select with `-i` (settings id), `-x` (index), `-n` (name), `-c` (class), or
  `--path` + optional `--ords`. Use exactly one selector and don't combine
  `--path` with another selector.
- A bare service name resolves `src\<Service>\__roblox_sync_settings.renium`;
  pass an explicit file with `-f` instead — never both.
- Duplicate names or paths return candidates; use `--ords`, `--id`, or `--index`.
- `--scope auto|metadata|property|attribute` controls what a property write
  targets. `auto` rejects names missing from the selected class. Use `property`
  only for a real newer or hidden Roblox property absent from the bundled
  schema, and `attribute` to create an attribute, for example
  `rbx bs Workspace -i editor:id -p Reviewed --scope attribute --bool true`.

Set references with `-j '{"_type":"Ref","settingsId":"editor:target"}'`;
clear with `--null`. Empty `changedPaths` means no push is needed.

`rbx bb` batches low-level reads. See `RENIUM/data.md` for operations and fields.

Search saved scripts without Studio:

```powershell
rbx script-search DataStoreService UpdateAsync --limit 20
rbx script-grep RemoteEvent --limit 100
rbx script-read src/ServerScriptService/Main.server.luau --start-line 40 --end-line 80
```

`script-search` matches all keywords case-insensitively. `script-grep` matches
literal lines and is case-sensitive by default. Limits cap results, not totals.
Line ranges are inclusive and one-based.

Inspect a closed place without opening Studio, or compare all of its projected
scripts with the current project:

```powershell
rbx q Place.rbxl -n RewardHandler
rbx q Place.rbxl --source "reward granted"
rbx cmp Place.rbxl
```

`q` reads RBXL/RBXLX directly. `cmp` ignores line-ending-only changes and
duplicate sibling order, and reports changed, missing, and extra scripts.

RBXM and requested `bb` properties may materialize class defaults. Check the
source `.renium` store before treating one as an override.

Export/import whole models and places:

```powershell
rbx bytecode-export-model Workspace -i editor:id -o model.rbxm
rbx bytecode-import-model model.rbxm --service Workspace
rbx export-place -o place.rbxl
rbx view model.rbxm --json                      # inspect a model without importing it
rbx sourcemap --stdout                          # print the complete sourcemap
rbx bytecode-repack                             # repack outdated stores and packages
```

`sm` writes `sourcemap.json` unless `--stdout` is used. Repacking changes only
old stores. `view` accepts stores and models, not places.

## Links (shared code across places)

A renium-link mirrors a local, Git, or Wally source to several targets. Links
are read-only by default; `--writable` preserves local edits.

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

Links use `renium-link.json`, `.renium/link-cache`, and the committed
`.renium/link.lock.json`. Override the cache with `--cache-dir`, `cacheDir`, or
the editor setting.

Third-party `.renium` packages may contain scripts, properties, and
`PackageLink`s. `lkp` packs a subtree. Package deletion can reject active uses,
delete them, or keep editable copies. `bpack` repacks stores and local packages.

Linked models keep local names and transforms when refreshed.

Linked editor files show an `L` badge and open read-only.

## Version control

A synced project is a normal Git repository. Run once:

```powershell
rbx vc-init                                            # in the project root
rbx vc-init --remote https://github.com/you/your-game  # also set origin
rbx vc-init --skip-git                                 # only write policy files
```

This safely sets ignore rules, LF policy, and `.renium` diff/merge drivers.

`git diff` renders stores as text. Merges operate per instance/property;
same-property conflicts report both values. Resolve through Git or
`rbx vc-merge <base> <ours> <theirs> --prefer theirs -o <file>`.

Typical flow:

```text
edit in Studio / editor  ->  renium syncs src/         ->  git commit + push
git pull                 ->  rbx lk (--strict in CI)   ->  rbx push to Studio
```

Git operates only on project files.

## Inspecting a .renium file

```powershell
rbx view src\SoundService\__roblox_sync_settings.renium   # text tree
rbx view links\ui-kit.renium --json                       # structured JSON
```

The extension opens `.renium` files with the same decoder.

## Wally packages

`rbx wally` installs and imports Wally packages. Unchanged lockfiles are no-ops.

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

PowerShell 5.1 mangles inline JSON. Pipe it instead:

```powershell
'{"ops":[{"type":"counts"}]}' | rbx batch Workspace -J -
```

## Command aliases

| Short | Descriptive alias | Short | Descriptive alias |
|---|---|---|---|
| `fmt` | `fmt-project` | `pv` | `project-validate` |
| `xp` | `explain-path` | `cfg` | `config` |
| `ad` | `adapters` | `ir` | `import-rojo` |
| `init` | `project-init` | `build` | `build-project` |
| `q` | `query-place` | `cmp` | `compare-place` |
| `dr` | `doctor` | `docs` | `open-docs` |
| `dm` | `daemon` | `bd` | `bridge-daemon` |
| `so` | `studio` | `ro` | `studio-open` |
| `sx` | `studio-close` | `status` | `studio-status` |
| `up` | `upload-place` | `upd` | `update` |
| `oc` | `cloud` | `sb` | `syncback` |
| `ip` | `import-path` | `cr` | `create` |
| `cp` | `clone` | `mv` | `move` |
| `rn` | `rename` | `rm` | `remove` |
| `upl` | `unlink-package-link` | `mip` | `import-model` |
| `pd` | `package-desync` | `pp` | `package-publish` |
| `pu` | `package-update` |  |  |
| `mep` | `export-model` | `tst` | `test` |
| `x` | `export-snapshots` | `pl` | `pull` |
| `ps` | `push` | `lon` | `live-start` |
| `lof` | `live-stop` | `lst` | `live-status` |
| `rp` | `retry-pending` | `dp` | `discard-pending` |
| `ed` | `explorer-daemon` | `src` | `bridge-get-source` |
| `f` | `find` | `tr` | `tree` |
| `in` | `inspect` | `bb` | `batch` |
| `bg` | `get-property` | `bs` | `set-property` |
| `bss` | `set-source` | `ba` | `add` |
| `bcl` | `bytecode-clone-instance` | `br` | `bytecode-remove-instance` |
| `bt` | `bytecode-editor-targets` | `bdp` | `bytecode-desync-package-link` |
| `bem` | `bytecode-export-model` | `bim` | `bytecode-import-model` |
| `bep` | `export-place` | `pdp` | `place-desync-package-link` |
| `bpack` | `bytecode-repack` | `ims` | `import-service` |
| `ss` | `script-search` | `sg` | `script-grep` |
| `sr` | `script-read` | `me` | `multi-edit` |
| `l` | `luau` | `lc` | `execute-client-luau` |
| `co` | `console` | `cs` | `clients` |
| `play` | `playtest` | `dev` | `device` |
| `sc` | `screenshot` | `pr` | `press` |
| `clk` | `click` | `ky` | `key` |
| `ui` | `user-interface` | `ty` | `type` |
| `wait` | `wait-until` | `go` | `goto` |
| `inp` | `input` | `rs` | `record-start` |
| `re` | `record-end` | `rv` | `review` |
| `st` | `studio-change-state` | `prop` | `apply-editor-property` |
| `del` | `apply-editor-delete` | `rev` | `editor-revert` |
| `setup` | `setup-renium` | `as` | `asset-search` |
| `ai` | `asset-insert` | `gm` | `generate-model` |
| `js` | `job-status` | `iu` | `image-upload` |
| `is` | `image-store` | `si` | `import-snapshots` |
| `sm` | `sourcemap` | `v` | `view` |
| `vci` | `vc-init` | `vct` | `vc-textconv` |
| `vcm` | `vc-merge` | `wally` | `sync-wally-packages` |
| `lk` | `link-apply` | `lkb` | `link-break` |
| `lks` | `link-status` | `lka` | `link-add` |
| `lkp` | `link-pack` | `lkd` | `link-delete-package` |
| `pa` | `place-add` | `pn` | `place-rename` |
| `po` | `place-reorder` |  |  |

Launchers only locate Renium and forward arguments.

## Building from source

Build from source:

```powershell
cargo build --locked --release --manifest-path tools\renium\Cargo.toml
```

If the executable is locked, stop the daemon:

```powershell
Get-Process renium -ErrorAction SilentlyContinue | Stop-Process -Force
```

Build all release artifacts into `dist/`:

```powershell
.\tools\build-release.ps1 -LocalBuild
```

Public builds omit `-LocalBuild` and require a clean checkout, license, and VS
Code publisher.

## Good to know

- Prefer settings IDs; names can repeat.
- For model pivots the property is `WorldPivot`.
- `rbx bd` stays alive until stopped. `--editor-stdio` exits when its editor
  closes stdin.
- Live Sync sends file edits to Studio and Studio edits to files. Event
  listeners remain active through editor disconnects.
- Live Sync starts by reconciling both sides against their last common state.
  Conflicts remain pending unless a Studio or editor preference is configured.
- Before changing a linked package descendant, Renium marks that package
  Changed and reports its path. Publishing remains an explicit choice. Renium
  preserves the PackageLink itself and never treats deleting it as package
  desynchronization. `pu` discards package changes and updates to the latest
  published version.
- Stable IDs preserve reparenting without duplication.
- Files → Studio batches create one undo step and stop on the first error.
- Snapshot imports move stale generated paths to `.renium/import-backups/`.
- Script comparison ignores CRLF/LF-only differences.

## Known limitations

- Unsupported: `TextChatMessage.Timestamp` and Studio-only `QDir`/`QFont`
  settings fields. `Axes`, `Faces`, and `Ray` sync.
- `Content` preserves URI and `None`; unsupported `Object` and `Opaque` sources
  stop export.
- Infinity, negative infinity, and NaN round-trip through tagged floats.
