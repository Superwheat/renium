# Roblox Cloud and creator assets

Read `RENIUM.md` first. Creator Store search, documentation reads, and local image validation don't need Studio. Asset insertion and generation change only the matching live Edit runtime until those changes are pulled or the place is saved.

Remove temporary results through `rbx l`, using the returned path. Model generation returns a `jobId`; status is `running`, `succeeded`, or `failed`. If `job-status --wait-seconds` expires first, it returns `running` successfully so the same job can be checked later. `image-store` validates a local PNG, JPEG, BMP, or TGA up to 5 MiB without uploading it. Filtered `http-get` body lines start with their readable-document line numbers; match counts count matching lines.

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
