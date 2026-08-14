# Renium automation

The Renium installer adds both `renium` and `rbx` to `PATH`. Use the compact `rbx` automation API for Studio and project operations. If the current process has not picked up the new `PATH`, use `%USERPROFILE%\.renium\bin\rbx.cmd` on Windows or `~/.renium/bin/rbx` on macOS and Linux; do not search extension folders.

## Bind once

Bind the intended project and place before every sequence. The returned `r.id` is the daemon-local context ID used as `CX` below.

```powershell
rbx a bind . 101945566570840
rbx a context CX
```

The place selector may be a place ID or `gameId:placeId`. If multiple Studio runtimes match, inspect the candidate IDs in `e.d.candidates`, put `root`, `place`, and the exact `runtime` in `bind.json`, then run `rbx a bind -J bind.json`. For an empty folder, set `bootstrap:true` in the bind payload; that context permits `project-init`, `project-validate`, and `cloud` until the project is created. A context becomes stale after daemon restart, project identity changes, runtime disconnect, or a plugin rebuild. Bind again after `stale_cx`.

## Project files

A single-place project keeps editable scripts under `src`. A multi-place project has `renium.experience.json` and one source tree under `places/<alias>/src` per place. Bind the experience root and place ID; the returned context resolves the right source tree, so later operations must not infer it from the shell directory.

Edit normal script files directly. Do not read, decode, edit, move, or replace files in `.renium`, and do not edit `sourcemap.json`; both are generated. Query or mutate their stored DataModel through the operations below.

## Operations

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

`pull` writes Studio into project files. `push` writes project files into Studio. Live sync stays two-way. When an operation returns `rejected` with `e.n` set to `review-prepare`, the exact operation needs a receipt before `review-apply` can execute it. The `rbx a` wrapper handles that receipt for direct commands.

## Sync and read

```powershell
rbx a pull CX
rbx a push CX
rbx a find CX -J find.json
rbx a tree CX -J tree.json
rbx a inspect CX -J inspect.json
rbx a bb CX Workspace -J ops.json
```

Start with a bounded high-level query. Example `find.json`:

```json
{ "service": "Workspace", "className": "Script", "limit": 5, "fields": "lookup" }
```

Example `tree.json`:

```json
{ "service": "Workspace", "name": "StoredData", "depth": 2, "limit": 100 }
```

Example `inspect.json` after a query returned a stable ID:

```json
{ "service": "Workspace", "settingsId": "editor:id", "fields": "brief,prop:Name,attr:Tags" }
```

Prefer `service` for project reads and edits. Use `file` only when a specific settings file is required, and never provide both in one payload.

Use `batch` or `bb` for several low-level reads in one request. Example `ops.json`:

```json
{
  "ops": [
    { "type": "search", "q": "Door", "limit": 5, "fields": "lookup" },
    { "type": "counts" }
  ]
}
```

Batch op types are `counts`, `service`, `search`, `find`, `children`, and `instance`. Useful field groups:

```text
lookup = id,n,c,path
tree   = id,n,c,cc,ch
brief  = id,n,c,path,cc
prop:X = one property
attr:X = one attribute
```

Batch requests also accept the compact aliases `op` or `kind` for `type`, `rid` for `requestId`, `q` for `query`, `l` for `limit`, `id` for `settingsId`, `x` for `index`, `n` for `name`, `c` for `className`, `pid` for `parentSettingsId`, `path` for `pathSegments`, `ords` for `pathOrdinals`, `props` for `properties`, and `attrs` for `attributes`.

For a duplicate path, include ordinals in a batch op:

```json
{ "ops": [{ "type": "instance", "path": ["Workspace", "Door"], "ords": [2], "fields": "brief" }] }
```

Request only the values needed. `find` also accepts `name`, `className`, `parentSettingsId`, `tag`, repeated `property` filters, and repeated `attribute` filters. A filter is either a name or `NAME=JSON`.

Script operations read saved files in the bound source tree directly, without asking Studio to re-read its scripts. `script-search` takes comma-separated or array `keywords` and optional `limit`; every keyword must match the path or source. `script-grep` takes a literal `query`, optional `caseInsensitive`, and optional `limit`. Both return source paths. Pass one returned `path` to `script-read`, with optional one-based inclusive `startLine` and `endLine`.

```json
{ "keywords": ["DataStoreService", "UpdateAsync"], "limit": 20 }
{ "query": "RemoteEvent", "caseInsensitive": false, "limit": 100 }
{ "path": "src/ServerScriptService/Main.server.luau", "startLine": 40, "endLine": 80 }
```

## Selectors and edits

Read and bytecode edit operations select one instance with `settingsId`, `index`, `name`, `className`, or `pathSegments` plus optional `pathOrdinals`. Do not combine a path with another selector. Project-aware clone, move, and remove operations require `settingsId`. Duplicate names and paths must use a stable ID or ordinals; find the ID again after reconnecting or doing a full sync.

The edit operations accept these main payload fields:

- `get-property`: `service`, one selector, `property`, optional `scope`.
- `set-property`: the same fields plus `value`.
- `set-source`: `service`, one selector, and either `source` or `sourceFile`.
- `add`: `service`, `name`, `className`, and optional `parentSettingsId`.
- `clone`: `service`, `settingsId`, and `parentSettingsId`.
- `move`: `service`, `settingsId`, `parentSettingsId`, and optional `targetService`.
- `remove`: `service` and `settingsId`.
- `multi-edit`: `filePath` is the live Studio script path and `edits` contains exact `oldString`/`newString` replacements with optional `replaceAll`; `className` creates a missing Script, LocalScript, or ModuleScript.

Example `set-property.json`:

```json
{ "service": "Workspace", "settingsId": "editor:id", "property": "DisplayName", "value": "VIP Man" }
```

Store edits change project files. Push only selected edited files immediately with `push-selected.json`:

```json
{ "changedPaths": ["src/ServerScriptService/Example.server.luau"] }
```

```powershell
rbx a set-property CX -J set-property.json
rbx a push CX -J push-selected.json
```

Use `editor:true` only with `set-property` or `remove` when the operation must target the bound live Studio instance directly. Verify every mutation with `inspect`, `get-property`, or another bounded read.

`revert` restores project data by `path` or by `service` plus `settingsId`; run `push` separately when the restored files should be sent to Studio. Model operations use these fields:

- `import-model`: `service`, `parentSettingsId`, `model`, optional `overridePackages`.
- `export-model`: `service`, `settingsId`, `output`, optional `format` (`rbxm` or `rbxmx`).
- `export-place`: `output`, optional `services` and `format` (`rbxl` or `rbxlx`).
- `import-snapshots`: optional `snapshotDir`, `services`, `threads`, and `noProjectWrite`.
- `export-snapshots`: the same export controls as `pull`, but it does not import the snapshots into source files.
- `sourcemap`: optional `output`, `stdout`, `absolutePaths`, and `filters`.

## Live sync

Live sync is always two-way. Choose its initial direction explicitly: run `pull` first for Studio priority, `push` first for editor priority, or neither for no initial sync. Then start the watcher:

```powershell
rbx a live-start CX -J live.json
rbx a live-status CX
rbx a retry-pending CX
rbx a discard-pending CX
rbx a live-stop CX
```

Example `live.json`:

```json
{ "services": "Workspace,ReplicatedStorage,ServerScriptService" }
```

## Play, console, screenshots, and recordings

`studios` returns matching edit runtimes and the bound runtime's play server and client entries. Example `play.json`:

```json
{ "players": 2, "mode": "play" }
```

Example `server-luau.json`:

```json
{ "code": "print(game.JobId)" }
```

Example `client-luau.json`:

```json
{ "player": "2", "code": "print(game.Players.LocalPlayer.Name)" }
```

```powershell
rbx a studios CX
rbx a play-start CX -J play.json
rbx a luau CX -J server-luau.json
rbx a luau CX -J client-luau.json
rbx a console CX -J console.json
rbx a shot CX -J shot.json
rbx a record-start CX -J record-start.json
rbx a record-end CX -J record-end.json
rbx a play-stop CX
```

Without `player`, `luau` runs on the edit runtime or play server. `player` accepts a player name or one-based client index. Luau returns values and captured output; compile errors, runtime errors, and timeouts fail the operation. Keep snippets action-first and direct, avoid setup and helper wrappers unless needed, and poll only values that can be absent. Prefer `wait()` over `task.wait()` when they are equivalent. `console.json` may use `player`, `limit`, `sinceSeq`, `grep`, and `level`. `shot.json` may use `player` and `output`, or `studio:true` for the edit viewport. For a temporary Studio camera view, provide three-number `cameraPosition` and `lookAt` arrays together; Renium restores the exact camera state after capture. Screenshots do not require Studio or the play client to have focus. Each Luau execution stops retained threads started by the previous execution in the same context.

`record-start` captures one exact Studio or play-client window as an animated WebP without activating it or capturing any other application. It accepts `player`, `studio`, `client`, `output` ending in `.webp`, `fps` from 1 through 30, `maxSeconds` from 1 through 300, and `quality` from 0 through 100. Save the returned `recordingId`, then pass it to `record-end`; that call stops the recorder, finishes the file, and returns its absolute path. Recordings contain video only.

`record-start.json`:

```json
{ "player": "2", "output": "test-clip.webp", "fps": 12, "maxSeconds": 60, "quality": 80 }
```

`record-end.json`:

```json
{ "recordingId": "RECORDING_ID_FROM_RECORD_START" }
```

## UI, world, and device input

Inspect visible UI before interacting. Prefer a returned path or ID over coordinates. Relevant payloads are:

- `ui`: optional `player`, `limit`, and `includeOffscreen`.
- `press`: `path` or `id`, optional `player`, `world`, `right`, and `hold`.
- `click`: `x`, `y`, and optional `player`, `right`, and `hold`.
- `key`: `key` and optional `player` and `holdMs`.
- `type`: `text`, optional TextBox `path`, `player`, and `enter`.
- `goto`: `target` or `pos`, optional `player`, `tp`, `timeout`, and `speedMultiplier` from 0.1 through 10.
- `input`: `actions` is an ordered array of key, text, mouse, scroll, and bounded wait actions; optional top-level `player` selects one exact client window.
- `wait`: `condition`, optional `player` or `client`, `timeout`, and `interval`.
- `device`: `action` is `list`, `status`, `set`, or `stop`; setting accepts `device`, `orientation`, `scalingMode`, `resolution`, and `pixelDensity`.

```powershell
rbx a ui CX -J ui.json
rbx a press CX -J press.json
rbx a type CX -J type.json
rbx a goto CX -J goto.json
rbx a wait CX -J wait.json
rbx a device CX -J device.json
rbx a input CX -J input.json
```

Example payloads:

```json
{ "player": "2", "limit": 100 }
{ "player": "2", "path": "PlayerGui.Shop.BuyButton" }
{ "player": "2", "path": "PlayerGui.Chat.Box", "text": "hello", "enter": true }
{ "player": "2", "actions": [{ "type": "move", "path": "PlayerGui.Shop.BuyButton" }, { "type": "mouse-down" }, { "type": "wait", "ms": 80 }, { "type": "mouse-up" }, { "type": "key-press", "key": "E" }] }
```

`ui` returns on-screen controls unless `includeOffscreen:true` is set. A duplicate path may use `Name[n]`; an ambiguous path fails with candidates, while one visible match is selected. `press` scrolls a control into view. For a world target, use `goto` before `press` with `world:true`; world presses fail when the target remains off-screen. Injected clicks cannot trigger `ClickDetector` because Roblox does not expose that input path. `input` action types are `key-down`, `key-up`, `key-press`, `text`, `move`, `mouse-down`, `mouse-up`, `click`, `scroll-up`, `scroll-down`, and `wait`. A mouse action may use `path` or `x` and `y`; later actions reuse the last position. All keyboard and mouse events are posted only to the selected Studio client process and window. Renium never moves the system cursor, sends global input, or activates another application. Keys include standard letters, digits, navigation keys, modifiers, punctuation, and F1 through F15.

Device emulation uses Studio's simulator without focus or ribbon interaction. While it is active, `shot` defaults to the simulated viewport. Named and stable device IDs are accepted, including notched devices for safe-area checks.

Example `goto.json`, `wait.json`, and `device.json` payloads:

```json
{ "player": "2", "target": "Workspace.Shop.Door" }
{ "player": "2", "condition": "workspace:GetAttribute('Ready') == true", "timeout": 10 }
{ "action": "set", "device": "iPhone 16 Pro", "orientation": "portrait", "scalingMode": "fit" }
```

## Creator assets and generation

`asset-search` searches the public Creator Store without starting Studio. Use `scope:"user"` for a user's Creator Inventory; that scope needs `ROBLOX_API_KEY` and accepts `userId`, or reads the signed-in Studio user when omitted. Public Roblox APIs do not expose group or universe Creator Inventory to third-party plugins, so those scopes return `unsupported` instead of guessing or using private endpoints.

```json
{ "scope": "creator-store", "query": "tree", "assetType": "Model", "maxResults": 10 }
{ "scope": "user", "userId": 123, "assetType": "MODEL", "maxResults": 25 }
```

`asset-insert` calls Studio's supported insertion APIs. It accepts `assetId`, `assetType`, optional `name`, and optional `parentPath`. Models, packages, meshes, images, decals, audio, video, and animations are supported. `generate-model` uses the plugin-accessible `GenerationService:GenerateModelAsync` API and returns a `jobId`; poll `job-status`, or pass `waitSeconds` from 0 through 120 for one bounded wait. It accepts `prompt`, optional `imageAssetId`, `size` as three numbers, `maxTriangles` from 12 through 20000, `generateTextures`, up to eight named `parts`, `name`, and `parentPath`.

```json
{ "assetId": 123, "assetType": "Model", "parentPath": "Workspace", "name": "Tree" }
{ "prompt": "low-poly red fire hydrant", "maxTriangles": 4000, "parentPath": "Workspace" }
{ "jobId": "returned-job-id" }
```

`image-store` validates a local PNG, JPEG, BMP, or TGA file up to 5 MiB and returns its resolved path for later operations. `image-upload` accepts one through twenty images and returns each asset ID and `rbxassetid://` URI. HTTP URLs use the locally installed plugin's supported `AssetService` upload API by default, under the signed-in Studio user and without a prompt or focus change; the response is a `jobId` for `job-status`. Local files, explicit `userId` or `groupId`, and `via:"open-cloud"` use Roblox Open Cloud and require `ROBLOX_API_KEY` before the daemon starts.

```json
{ "path": "images/reference.png" }
{ "images": ["images/a.png", "https://example.com/b.jpg"], "userId": 123, "name": "Reference" }
```

Roblox restricts material generation and its internal primitive `ProceduralModel` generator to Roblox-authored scripts. Renium exposes the public third-party-plugin model generator and ordinary material property editing, but does not disguise inaccessible internal APIs as supported operations.

`http-get` fetches Roblox Creator documentation over HTTPS without Studio. It accepts optional `query`, `contextLines`, and `returnFull` fields. It accepts only `create.roblox.com` and the official `Roblox/creator-docs` repository, so it is a documentation reader rather than a generic network escape.

```json
{ "url": "https://create.roblox.com/docs/reference/engine/classes/GenerationService" }
```

## Project and place management

Use a bootstrap context for `project-init`, then bind the created project again. `project-validate` checks the bound project without starting Studio. `place-add` takes `placeId`, `name`, and optional `gameId`, `alias`, and `root`. `place-rename` takes `placeId` and `alias`. `place-reorder` takes `order`, an array containing every published place ID exactly once.

`studio-open`, `studio-close`, and destructive replacement require a review receipt. They never run as an automatic recovery route after another operation fails.

## Roblox Open Cloud

`cloud` calls `https://apis.roblox.com` directly and doesn't start or wait for Studio. Set `ROBLOX_API_KEY` in the environment before starting the Renium daemon. The key must never be put in a payload, project file, command argument, or output. Use `keyEnv` only when the key is stored under another environment variable. Set `anonymous:true` only for an endpoint that the Roblox reference marks as unauthenticated.

Send several independent requests in one daemon call. Renium reuses its HTTPS connection, percent-encodes `pathParams`, inserts the bound experience and place IDs for `{universe}` and `{place}`, and returns responses in request order. Don't batch dependent writes because a later failure doesn't undo earlier requests.

Example `creator-search.json`:

```json
{
  "anonymous": true,
  "requests": [{
    "method": "GET",
    "path": "/toolbox-service/v2/assets:search",
    "query": {
      "searchCategoryType": "Model",
      "query": "tree",
      "maxPageSize": 5
    }
  }]
}
```

Example `data-store.json` uses the bound context's universe ID and safely escapes store, scope, and entry IDs:

```json
{
  "requests": [
    {
      "id": "stores",
      "method": "GET",
      "path": "/cloud/v2/universes/{universe}/data-stores",
      "query": { "maxPageSize": 25 }
    },
    {
      "id": "profile",
      "method": "GET",
      "path": "/cloud/v2/universes/{universe}/data-stores/{store}/scopes/{scope}/entries/{entry}",
      "pathParams": { "store": "PlayerData", "scope": "global", "entry": "123" }
    }
  ]
}
```

`query` accepts scalar values or arrays. JSON request bodies go in `body`; conditional writes may use `ifMatch` or `ifNoneMatch`. Responses include HTTP `status`, parsed `body`, optional request `id`, ETag, last-modified, retry, and rate-limit headers. HTTP failures include the request index, status, headers, and Roblox error body in `e.d`.

```powershell
rbx a cloud CX -J creator-search.json
rbx a cloud CX -J data-store.json
```

## Payload files

Put structured parameters in a JSON file. Use stdin when the payload is generated:

```powershell
Get-Content .\set-property.json | rbx a set-property CX -J -
```

```sh
rbx a set-property CX -J - < ./set-property.json
```

Never put structured JSON directly in a shell argument.

## Compact protocol fields

- Request: `v` protocol version, `id` request ID, `op` numeric opcode, `cx` bound context, `p` parameters.
- Success: `ok:1`, `ms` elapsed milliseconds, `r` result.
- Failure: `ok:0`, `e.c` stable code, `e.m` message, `e.rt` retry flag, `e.n` next operation, `e.d` details.
- Open Cloud failures use `cloud_auth` for missing or rejected credentials and `cloud_http` for Roblox or transport failures; `conflict` covers HTTP 409 and 412.
- Batch top-level fields: `f` settings file, `s` service, `rs` results, `t` op type, `q` request ID.
- Instance fields: `id`, `x` index, `n` name, `c` class, `pid`/`px` parent, `path`, `ords`, `cc` child count, `ch` child IDs, `src`, `props`, and `attrs`. Request only the fields needed.

Use a stable instance ID after `find` or `tree`. Duplicate paths require ordinal selectors. Never guess the project, place, runtime, sync direction, or duplicate instance. Treat `ok:0` or a nonzero wrapper exit as a failed operation; use the returned error and never retry by switching to a different operation.
