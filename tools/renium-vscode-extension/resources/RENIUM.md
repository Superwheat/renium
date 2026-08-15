# Renium automation

Use `rbx`; the installer adds `rbx` and `renium` to `PATH`. If this process has an old `PATH`, use `%USERPROFILE%\.renium\bin\rbx.cmd` on Windows or `~/.renium/bin/rbx` on macOS and Linux. Never search editor extension folders.

## Rules

- Use direct `rbx` commands for normal work. They reuse the editor daemon or connect to Studio themselves.
- Don't start `rbx bd`, inspect daemon state, inspect CLI usage, or bind a context before a direct command.
- Don't create JSON payload files. Use direct flags; pipe JSON to `rbx a ... -J -` only for structured operations without a direct command.
- Read before writing. Use a stable settings ID when names repeat, then verify the effect.
- Never edit `.renium` or `sourcemap.json` by hand.
- Don't launch, close, or replace Studio as a fallback. Those actions require an explicit review receipt.

## Projects and targeting

Single-place projects use `src`. Multi-place projects use `places/<alias>/src` and `renium.experience.json`. Run commands anywhere below the project root. Pin an open place with global `--place <name|placeId|gameId:placeId>` or `RENIUM_PLACE`; commands refuse ambiguous matches instead of guessing.

Edit `.lua` and `.luau` files directly. Renium stores other instance data in generated `.renium` files. Use the commands below to inspect or edit that data.

## Read and edit project data

```powershell
rbx find Workspace -n Door --limit 5
rbx find ServerScriptService -c Script --limit 5
rbx tree Workspace Door --depth 2 --limit 100
rbx inspect Workspace -i editor:id
rbx bg Workspace -i editor:id -p Name
rbx bs Workspace -i editor:id -p DisplayName --str "VIP Man"
rbx bss ServerScriptService -i editor:script --source-file .\Main.server.luau
rbx ba Workspace -n NewModel -c Model
rbx bcl Workspace -i editor:source -I editor:parent
rbx br Workspace -i editor:id
```

Selectors are `-i` for settings ID, `-x` for index, `-n`/`-c` for name/class, or `--path` with optional `--ords`. Don't combine a path with another selector. Use a service name first; use `-f` only for one explicit store, never both.

Batch related reads once. The compact response has one flat result per request in top-level `rs`; it doesn't nest one result inside another.

```powershell
'{"ops":[{"type":"search","q":"Door","limit":5,"fields":"lookup"},{"type":"counts"}]}' | rbx bb Workspace -J -
```

Field presets: `lookup=id,n,c,path`, `tree=id,n,c,cc,ch`, `brief=id,n,c,path,cc`; request one property with `prop:Name` or attribute with `attr:Tags`.

Inspect models without importing them:

```powershell
rbx view model.rbxm --json
rbx view model.rbxmx
```

## Sync

`pull` means Studio to files. `push` means files to Studio. Live sync remains two-way.

```powershell
rbx a bind . 101945566570840
rbx a pull CX
rbx a push CX
rbx a live-start CX
rbx a live-status CX
rbx a retry-pending CX
rbx a discard-pending CX
rbx a live-stop CX
```

Replace `CX` with the `r.id` returned by `bind`. Bind the experience root and place ID. Contexts expire after daemon restart, project identity changes, runtime loss, or an incompatible plugin rebuild; bind again after `stale_cx`.

For initial live-sync priority, pull before `live-start` for Studio priority, push first for editor priority, or do neither for no initial sync. To push only selected files, pipe `{"changedPaths":["src/ServerScriptService/Main.server.luau"]}` to `rbx a push CX -J -`.

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

Outside Play, `rbx l` runs in the edit plugin context. It has the edit DataModel but no normal Play-client `LocalPlayer` or `PlayerGui`. Start Play before requiring runtime client code. During Play, `rbx l` targets the server and `rbx lc ... <name|index>` targets one client. Luau compile errors, runtime errors, and timeouts return nonzero.

## UI, world, capture, and devices

```powershell
rbx ui -p 2
rbx press "PlayerGui.Shop.BuyButton" -p 2
rbx type "hello" --path "PlayerGui.Chat.Box" --enter -p 2
rbx click 450 323 -p 2
rbx key E -p 2
rbx goto "Workspace.Shop.Door" -p 2
rbx wait "workspace:GetAttribute('Ready') ~= nil" -c -t 20
rbx shot --studio -o studio.png
rbx shot -p 2 -o client.png
rbx device set "iPhone 16 Pro" --orientation portrait --scaling fit
rbx device status
rbx device stop
```

Run `ui` first and prefer its path or ID over coordinates. Duplicate names use `Name[n]`; ambiguity returns candidates. `press --world` needs an on-screen target, so use `goto` first. Injected clicks can't fire `ClickDetector`; use a `ProximityPrompt` or game input path.

Input targets one Play window without moving the system cursor or taking focus. The orange native shield stops physical input from interrupting an active sequence. Screenshots and H.264 MP4 recordings capture only the selected Studio/client window.

Recording is structured because start returns an ID needed by stop:

```powershell
'{"player":"2","output":"test.mp4","fps":12,"maxSeconds":60,"quality":80}' | rbx a record-start CX -J -
'{"recordingId":"ID_FROM_START"}' | rbx a record-end CX -J -
```

## Models, places, links, and scripts

```powershell
rbx bem Workspace -i editor:id -o model.rbxm
rbx bim model.rbxm --service Workspace
rbx bep -o place.rbxl
rbx sm
rbx lk
rbx wally --realms shared
```

`bem`/`bim` export and import model trees. `bep` builds a place from project data. `sm` regenerates the sourcemap for all mapped instances, not only scripts. `lk` applies Renium links; linked package mirrors are read-only unless configured writable.

Search saved script files without asking Studio to read them again:

```powershell
'{"keywords":["DataStoreService","UpdateAsync"],"limit":20}' | rbx a script-search CX -J -
'{"query":"RemoteEvent","limit":100}' | rbx a script-grep CX -J -
'{"path":"src/ServerScriptService/Main.server.luau","startLine":40,"endLine":80}' | rbx a script-read CX -J -
```

## Typed operations

Use `rbx a` only when the direct surface doesn't cover the operation. Payloads are JSON objects read from stdin with `-J -`; don't save them in the project. Bind once per sequence, then pass its context ID.

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

Creator Store search and documentation reads don't need Studio. Asset insertion and generation use the bound Studio runtime. Open Cloud uses `ROBLOX_API_KEY` from the daemon environment; never put a key in a command, payload, project file, or output. `cloud` accepts independent requests in one call and substitutes the bound `{universe}` and `{place}` path values.

Project creation and place management use `project-init`, `project-validate`, `place-add`, `place-rename`, and `place-reorder`; reorder uses published place IDs. Studio open/close, protected-property fallback, and destructive replacement use `review-prepare` and the returned receipt. Never retry a permanent error with a different operation.

Errors use stable codes. Rebind on `stale_cx`; choose a candidate on `ambiguous_place`; connect the matching Studio place on `no_studio` or `bridge_off`; correct the payload on `bad_req`. Only retry when `rt` is `1`, and retry the same operation at most once.

The complete human CLI and configuration reference is in `tools/renium/README.md`.
