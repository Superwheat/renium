# Renium automation

Use `rbx`; the installer adds `rbx` and `renium` to `PATH`. If this process has an old `PATH`, use `%USERPROFILE%\.renium\bin\rbx.exe` on Windows or `~/.renium/bin/rbx` on macOS and Linux. Never search editor extension folders.

## Rules

- Use direct `rbx` commands for normal work. They reuse the editor daemon or connect to Studio themselves.
- Don't start `rbx bd`, inspect daemon state, inspect CLI usage, or bind a context before a direct command.
- Don't create JSON payload files. Use direct flags; pipe JSON to `rbx a ... -J -` only for structured operations without a direct command.
- Read an existing target before changing it. Don't preflight a uniquely named temporary file or instance; create it, then reuse successful writes and returned IDs or paths without rereading them. Pull creates a new snapshot, so find existing IDs again afterward.
- After removing uniquely named temporary instances, confirm cleanup with one search for their shared name prefix; don't probe every deleted ID. If `br` returns `storeRemoved: true`, don't search that absent service.
- Don't read or edit `.renium` or `sourcemap.json` by hand; use `rbx`.
- `.renium/editor-history` is expected local revert data, not project content; don't inspect or restore its timestamps.
- Don't launch, close, or replace Studio as a fallback. Those actions require an explicit review receipt.
- Run one mutation command at a time and inspect its result before the next. Never chain edits, deletes, pulls, pushes, Undo, or Redo in one shell command.
- If a mutation fails, stop changing Studio. Verify the affected live roots before any pull, push, retry, Undo, Redo, package insertion, or recovery action.
- Don't rename, delete, or replace a Roblox package root to work around a failed edit. Use `desync-package-link` only when removing the package relationship is the intended change.

## Projects and targeting

Single-place projects use `src`. Multi-place projects use `places/<alias>/src` and `renium.experience.json`. Inside `places/<alias>`, commands infer that place. At the experience root, Renium uses the sole matching Studio place from the running daemon; otherwise put `--place <alias|placeId>` before the command. Studio commands also accept `gameId:placeId` and place names. Ambiguous commands list the valid places instead of searching a nonexistent root `src`.

Edit `.lua` and `.luau` files directly. Renium stores other instance data in generated `.renium` files. Use the commands below to inspect or edit that data.

Project commands such as `find`, `bg`, and `bs` read or edit saved files, not the live Studio tree. Use edit-context `rbx l` for a live Studio change, then pull it; push saved file changes to Studio.
`src/<Service>/...` maps to that same Roblox service, so query the service named by the file path.

## Read and edit project data

```powershell
rbx find Workspace -n Door --limit 5
rbx find ServerScriptService -c Script --limit 5
rbx tree Workspace Door --depth 2 --limit 100
rbx inspect Workspace -i editor:id
rbx bg Workspace -i editor:id -p Name
rbx bs Workspace -i editor:id -p DisplayName --str "VIP Man"
rbx bs Workspace -i editor:id -p Transparency --num 0.5
rbx bs Workspace -i editor:id -p Anchored --bool true
rbx bs Workspace -i editor:id -p Reviewed --scope attribute --bool true
rbx ba Workspace -n NewModel -c Model
rbx ba Workspace -I editor:parent -n NewPart -c Part
rbx bss Workspace -i editor:script --str "return 1"
rbx bcl Workspace -i editor:source -I editor:parent
rbx move Workspace -i editor:id -I editor:parent
rbx br Workspace -i editor:id
```

Property values use `--str`, `--num`, `--bool`, `--null`, or `-j` for another JSON value. `--null` removes the stored override; writing the Roblox default explicitly still stores an override. Automatic writes reject property names missing from the class. Use `--scope property` only for a real newer or hidden Roblox property absent from Renium's bundled schema, and `--scope attribute` to create an attribute.

Set or clear an instance reference with `-j '{"_type":"Ref","settingsId":"editor:target"}'` or `--null`. Ref objects can also use `pathSegments` plus `pathOrdinals` when an ID is unavailable.

Edit an existing project script file directly; don't run `bss` afterward. For a newly created temporary script, `bss --str` writes its source directly. Use `bss --source-file` to copy source from another file.

Selectors are `-i` for settings ID, `-x` for index, `-n` for name, `-c` for class, or `--path` with optional `--ords`. Use exactly one selector. Don't combine a path with another selector. Use a service name first; use `-f` only for one explicit store, never both.

Mutation results list only files whose bytes or paths changed in `changedPaths`. An empty list is a successful no-op and needs no push.

Batch related reads once. The compact response has one flat result per request in top-level `rs`; it doesn't nest one result inside another.
In batch fields, `src` is the source-file path; use `prop:Source` for exact script text.

```powershell
'{"ops":[{"type":"search","q":"Door","limit":5,"fields":"lookup"},{"type":"counts"}]}' | rbx bb Workspace -J -
```

Field presets: `lookup=id,n,c,path`, `tree=id,n,c,cc,ch`, `brief=id,n,c,path,cc`; request one property with `prop:Name` or attribute with `attr:Tags`.
Requested properties omitted from a `bb` node aren't serialized overrides; they use the Roblox class default.

Inspect models without importing them:

```powershell
rbx view model.rbxm --json
rbx view model.rbxmx --json
```

Use `--json` when exact script source and stable references are required. Plain model view summarizes source text. `view` accepts `.renium`, `.rbxm`, and `.rbxmx`, not place files; verify place contents from `bep`'s manifest and `sm --stdout` before comparing exported hashes.
RBXM is columnar, so decoding it can materialize a Roblox class default for an instance that never stored that property. Requested `bb` properties also return class defaults. Use `rbx view <store>.renium --json` to distinguish stored overrides before comparing model formats.

## Project configuration and adapters

```powershell
rbx fmt-project --check
rbx fmt-project
rbx explain-path data/config.json
rbx project-validate
rbx adapters build
rbx adapters build --check
rbx adapters syncback --preview
rbx adapters syncback
rbx import-rojo --project default.project.json --preview
rbx import-rojo --project default.project.json --apply
rbx import-path .\Shared.server.luau --path-json '["ServerScriptService","Shared"]' --dry-run
rbx import-path .\Shared.server.luau --path-json '["ServerScriptService","Shared"]'
rbx import-path .\SharedFolder --destination src\ReplicatedStorage\Shared --dry-run
```

`build` maps configured source files into project instances. `syncback` writes supported two-way instance edits back to their adapter sources. Use `--check` or `--preview` before a write when only validation or a plan is needed; don't use `watch` for one-off agent work.
`project-validate` checks the complete project offline. `import-rojo` converts one Rojo project file or a folder containing exactly one into a formatted `renium.project.jsonc`; preview before applying.
`import-path` copies one file by Roblox path or a directory by project-relative destination. Preview first; existing files are reported as `unchanged` or `overwrite`, and omitting `--dry-run` applies the listed actions.
Mounts use `{"source":"shared","target":"ReplicatedStorage.Shared","ownership":"read-only","optional":true}` in the project's `mounts` array. Ownership defaults to `exclusive`; optional missing sources project nothing. Normal reads include mounted instances, `bss` writes writable mounted scripts, and `explain-path` follows nested-project descendants.

`syncRules` map extra file types into instances. Rules are ordered and the last match wins; `suffix` strips a file suffix, `exclude` disqualifies that rule, and `use: "ignore"` suppresses the file. `globIgnorePaths` ignores matching project paths before projection.

```jsonc
{
  "syncRules": [
    { "pattern": "**/*.server.txt", "use": "serverScript", "suffix": ".server.txt" },
    { "pattern": "**/draft/**", "use": "ignore" }
  ],
  "globIgnorePaths": ["src/generated/**"],
  "filters": [
    { "action": "ignore", "direction": "files-to-studio", "class": "ModuleScript" },
    { "action": "include", "direction": "files-to-studio", "name": "Shared" },
    { "action": "ignore", "direction": "both", "glob": "Workspace/Generated/**", "property": "Source" }
  ]
}
```

Filter actions are `include` or `ignore`; directions are `files-to-studio`, `studio-to-files`, or `both`. Selectors are `glob`, `name`, `class`, `tag`, `attribute`, `property`, and `id`. Filters are ordered and last-match wins; `property` and `attribute` rules affect only that field. In `explain-path`, `owned` means a mapping claims the path, `ignored` means `globIgnorePaths` blocks it, and `selectedSyncRule` identifies the winning rule even when another setting suppresses its output.

## Sync

`pull` means Studio to files. `push` means files to Studio. The editor's Live Sync command runs both directions.

```powershell
rbx pull
rbx push --changed-path src/StarterGui/AuditClient.client.luau --no-review --yes
```

Run these from the active place folder. At an experience root with more than one place, add the global `--place <alias|placeId>` selector before the command. Renium starts or reuses the shared daemon and waits for the matching Studio runtime.

The CLI doesn't watch files. After project edits, pass each returned `changedPaths` entry to `rbx push --changed-path`; repeat the flag for multiple files. After Studio edits, pull. Use an unfiltered push only for a full place replacement. `live-*` controls the editor's change tracker; agents normally need only `live-status` or `discard-pending`.

## Play and Luau

```powershell
rbx play -s                         # ordinary Play; default for one-client checks
rbx play -s --players 1             # local server plus one separate client
rbx play -s --players 2             # local server plus two clients
rbx clients
rbx l "print(game.PlaceId)"         # Play server during a test
rbx lc "print(game.Players.LocalPlayer.Name)" 2
rbx co --player 2 -n 20
rbx play -x
```

Use ordinary Play unless the test needs a separate server runtime or multiple clients. `--players 1` is still a local-server test, not ordinary one-player Play.
Ordinary Play still reports its internal `play-server` and `play-client` bridges; `mode: "play"` confirms it isn't a local-server test.

Outside Play, `rbx l` runs in the edit plugin context. It has the edit DataModel but no normal Play-client `LocalPlayer` or `PlayerGui`. Start Play before requiring runtime client code. During Play, `rbx l` targets the server and `rbx lc ... <name|index>` targets one client. Luau compile errors, runtime errors, and timeouts return nonzero.

## UI, world, capture, and devices

```powershell
rbx ui -p 2
rbx press "Shop.BuyButton" -p 2
rbx type "hello" --path "Chat.Box" --enter -p 2
rbx click 450 323 -p 2
rbx key E -p 2
rbx key W --hold-ms 700 -p 2
rbx goto "Workspace.Shop.Door" -p 2
rbx goto --pos "745,40,510" -p 2
rbx wait "workspace:GetAttribute('Ready') ~= nil" -c -t 20
rbx shot --studio -o studio.png
rbx shot -p 2 -o client.png
rbx device list
rbx device set "iPhone 16 Pro" --orientation portrait --scaling fit
rbx device stop
```

Run `ui` first and reuse its `p` path exactly; paths are relative to `PlayerGui`, though a leading `PlayerGui.` is also accepted. Duplicate names use `Name[n]`; ambiguity returns candidates. `press --world` needs an on-screen target, so use `goto` first. Injected clicks can't fire `ClickDetector`; use a `ProximityPrompt` or game input path.

`goto` finishes within eight studs of its target so nearby interaction is possible; its result includes the final distance.

Input targets one Play window without moving the system cursor or taking focus. The orange native shield stops physical input from interrupting an active sequence. Screenshots and H.264 MP4 recordings capture only the selected Studio/client window.

Use device simulation only for mobile, resolution, or safe-area checks. Configure or stop it in Edit mode, never during Play, and never use it to repair a hidden normal viewport.
`device set` returns the resulting state, so don't call `device status` immediately afterward. Use `device status` to read existing state later. `device list` returns selection fields; use `device list --details` or `device status --details` only when native dimensions or density are needed.

Structured input uses `actions` with an `action` field, for example `{"player":"1","actions":[{"action":"click","path":"Shop.BuyButton"},{"action":"wait","ms":100}]}` piped to `rbx a input CX -J -`.

Start, act, and end in one shell call so planning time isn't recorded:

```powershell
rbx record-start -p 2 -o test.mp4
rbx key W --hold-ms 700 -p 2
rbx record-end
```

`record-end` stops the sole active recording; an optional recording ID checks that it is the expected one. End before screenshots, console reads, or other verification.

## Models, places, links, and scripts

```powershell
rbx bem Workspace -i editor:id -o model.rbxm
rbx bim Workspace --model model.rbxm --parent-settings-id editor:parent
rbx bep -o place.rbxl
rbx x -d snapshots --no-run-import
rbx im --snapshot-dir snapshots --project-root .
rbx sm
rbx sm --stdout
rbx bpack
rbx wally --realms shared
rbx wally --realms shared --force
```

Wally sync needs a working `wally` command; Aftman users must declare Wally for the project first. Use `--force` to reinstall and reimport packages that are already current. Add `--details` only when the full changed-path and instance-ID lists are needed.

`bem`/`bim` export and import model trees. `x` exports raw Studio snapshots; `im` imports them into project files. `bep` builds a place from project data. `sm` writes the sourcemap for all mapped instances, not only scripts; use `sm --stdout` when its contents are needed without creating a file. `bpack` rewrites project stores in the current format and reports which files changed; already-current files remain untouched.

Version control: run `rbx vc-init` once in a project to initialize Git and Renium's ignore, text-diff, and merge rules; rerunning it is safe. `rbx vc-textconv FILE.renium` renders one binary store as deterministic text. Git invokes `rbx vc-merge BASE OURS THEIRS` automatically for a conflicting `.renium` merge. Use normal or path-scoped `git status`; `--untracked-files=all` expands every generated package file.

Mirror one local source into a project target:

```powershell
rbx lka --id logger --source-type local --source links/Logger.luau --service ReplicatedStorage --path '["ReplicatedStorage","Shared","Logger"]'
rbx lk
rbx lks
rbx lkb --service ReplicatedStorage --path '["ReplicatedStorage","Shared","Logger"]' --remove
```

`lka` adds the target, `lk` materializes current source, and `lks` reports total and active target counts. Plain `lkb` temporarily detaches a target; `lkb --remove` also removes its link record. Both keep the target editable and externalize scripts embedded by a package. Local source paths are relative to the project root. Mirrors are read-only unless added with `--writable`; reuse each `rootSettingsId` returned by `lk` for the subtree root and `settingsIds` for its instances, and push returned `changedPaths` only when Studio also needs the update.

For a Git source, replace the source arguments with `--source-type git --source REPOSITORY --ref BRANCH_OR_COMMIT --subpath PATH`. Renium caches the repository and refreshes the requested ref on `lk`; use `lk --offline` only after that source has been cached.

Pack an existing subtree into a reusable project package, insert it elsewhere, then remove the package while keeping both materialized trees:

```powershell
rbx lkp --link-folder packages --id shared-widget --service ReplicatedStorage --path '["ReplicatedStorage","PackageSource"]'
rbx lka --id shared-widget --service ReplicatedStorage --path '["ReplicatedStorage","PackageCopy"]'
rbx lk --link shared-widget
rbx lkd --id shared-widget --action unlink-uses
```

`lkp` writes `packages/shared-widget.renium` and registers the packed subtree as its first target. `lka` can reuse that link id without repeating its source. `lkd --action delete-unused` refuses active uses, `delete-uses` removes them, and `unlink-uses` keeps them as ordinary editable project instances. All three delete the package and link.

Search saved script files without asking Studio to read them again:

```powershell
rbx script-search DataStoreService UpdateAsync --limit 20
rbx script-grep RemoteEvent --limit 100
rbx script-read src/ServerScriptService/Main.server.luau --start-line 40 --end-line 80
```

`script-search` matches files containing every keyword, without case sensitivity, and reports file counts. `script-grep` matches literal source text, is case-sensitive unless `--case-insensitive` is used, and reports line counts. Limits cap returned results while totals still cover the full project.

## Typed operations

Use `rbx a` only when the direct surface doesn't cover the operation. Payloads are JSON objects read from stdin with `-J -`; don't save them in the project. Bind once per sequence, then pass its context ID.
Use `rbx clients` or `rbx studios` to list Studio runtimes without binding.

<!-- automation-operations:start -->
- Context: `cap`, `bind`, `context`, `unbind`
- Sync: `pull`, `push`, `live-start`, `live-stop`, `live-status`, `retry-pending`, `discard-pending`
- Read: `find`, `tree`, `inspect`, `batch`, `script-search`, `script-read`, `script-grep`
- Edit: `get-property`, `set-property`, `set-source`, `add`, `clone`, `move`, `remove`, `revert`, `multi-edit`
- Files: `import-model`, `export-model`, `export-place`, `import-snapshots`, `export-snapshots`, `sourcemap`
- Studio: `studios`, `studio-status`, `studio-open`, `studio-close`, `luau`, `console`, `play-start`, `play-stop`, `shot`, `device`
- Input and capture: `ui`, `press`, `click`, `key`, `type`, `wait`, `goto`, `input`, `record-start`, `record-end`
- Project: `project-init`, `project-validate`, `place-add`, `place-rename`, `place-reorder`
- Review: `review-prepare`, `review-apply`, `review-reject`
- Roblox Cloud and creator assets: `cloud`, `asset-search`, `asset-insert`, `generate-model`, `job-status`, `image-upload`, `image-store`, `http-get`
<!-- automation-operations:end -->

Common structured payload fields follow their names: selectors use `service` plus one of `settingsId`, `index`, `name`, `className`, or `pathSegments`/`pathOrdinals`; file operations use `model`, `output`, or `changedPaths`; client operations use `player`. Batch accepts an `ops` array.

Creator Store search, documentation reads, and local image validation don't need Studio. Asset insertion and generation change only the matching live Edit runtime until those changes are pulled or the place is saved. Remove temporary results through `rbx l`, using the returned path. Model generation returns a `jobId`; status is `running`, `succeeded`, or `failed`. If `job-status --wait-seconds` expires first, it returns `running` successfully so the same job can be checked later. `image-store` validates a local PNG, JPEG, BMP, or TGA up to 5 MiB without uploading it. Filtered `http-get` body lines start with their readable-document line numbers; match counts count matching lines.

```powershell
rbx asset-search "wooden crate" --limit 5
rbx asset-insert 182451181 --parent Workspace --name AuditCrate
rbx generate-model "small wooden crate" --parent Workspace --name GeneratedCrate --size 4,4,4 --max-triangles 2000
rbx job-status JOB_ID --wait-seconds 30
rbx image-store assets/reference.png
rbx http-get "https://create.roblox.com/docs/reference/engine/classes/StudioDeviceSimulatorService" --query GetResolutionAsync --limit 1
```

Open Cloud uses `ROBLOX_API_KEY` from the daemon environment; never put a key in a command, payload, project file, or output. It accepts `requests` containing `method`, `path`, and optional `query`, `body`, `pathParams`, `ifMatch`, or `ifNoneMatch`. Paths can use bound `{universe}` and `{place}` values only for a published project with nonzero game and place IDs; otherwise provide explicit numeric path parameters. Pipe variable payloads through stdin:

```powershell
'{"requests":[{"method":"GET","path":"/cloud/v2/universes/{universe}/data-stores","query":{"maxPageSize":25}}]}' | rbx a cloud CX -J -
```

Image upload writes to the user's Roblox account, so run it only when the user asked for an upload. HTTP image URLs without ownership fields use the connected Studio account. Local files need Open Cloud ownership and `via:"open-cloud"`; results contain an asset ID or an asynchronous job result.

```powershell
'{"images":["https://example.com/image.png"],"name":"Reference"}' | rbx a image-upload CX -J -
'{"images":["assets/reference.png"],"userId":123,"via":"open-cloud","waitSeconds":30}' | rbx a image-upload CX -J -
```

Project creation and place management use `project-init`, `project-validate`, `place-add`, `place-rename`, and `place-reorder`; reorder uses published place IDs. Studio open/close, protected-property fallback, and destructive replacement use `review-prepare` and the returned receipt. Never retry a permanent error with a different operation.

```powershell
rbx a bind . --bootstrap
rbx a project-init CX
rbx a bind . <PLACE_ID>
rbx a place-add CX <PLACE_ID> "Place Name" --game-id <GAME_ID> --alias main
rbx a place-rename CX <PLACE_ID> lobby
rbx a place-reorder CX <PLACE_ID> <OTHER_PLACE_ID>
```

Rebind after `project-init`, `place-add`, `place-rename`, or `place-reorder` changes project identity.

Errors use stable codes. Rebind on `stale_cx`; choose a candidate on `ambiguous_place`; connect the matching Studio place on `no_studio` or `bridge_off`; correct the payload on `bad_req`. Only retry when `rt` is `1`, and retry the same operation at most once.
