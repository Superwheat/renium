# Save, publish, and troubleshoot

See [advanced](advanced.md) for the main workflow.

## Save and publish state

Synced work is in the project files and the open Studio session. `rbx status` reports `teamCreate` and `placeVersion` for the selected Edit place: with Team Create, Studio saves every edit to Roblox itself, scripts included, and other collaborators' Studios receive it; without it, only File > Save to Roblox or a publish does, and Renium cannot see whether that happened. Team Create reverts a reparent out of Workspace.Terrain within a second (a pull then shows the old parent); move such instances with Clone to the new parent plus Destroy of the original. Players get changes only from a publish. When a collaborator in Studio still sees old behaviour, the code is the suspect, not the sync: `rbx oc fetch` downloads the cloud copy to check what it holds. Report what is synced and whether you published; don't call the place saved or unsaved. Offer `rbx publish` when the user wants the changes live.

## Publishing places

```powershell
rbx publish --dry-run
rbx --place lobby publish
rbx publish --open-cloud
rbx publish --open-cloud --file build.rbxl --universe 123 --place-id 456
rbx publish --as 456 --universe 123 --key live
```

Publishing requires user authorization. Default: publish the selected Studio Edit
state to its existing place with the Studio login, without pushing files first.
Both Studio routes first wait up to 20 s for Live Sync to settle and refuse while
file changes are pending or a conflict is open (`--allow-pending` publishes Studio
as it is) or while Play runs (`rbx play -x`; `--as` also takes `--allow-play`).
Results report `liveSync`, `previousVersion` and `versionNumber`. When the place
refuses `SavePlaceAsync` (Save Place API off, Team Create), Renium runs Studio's own
Publish command and reads the result from Studio's log (Windows and macOS). Don't
change place settings or credentials to work around a refusal without authorization.

`--as PLACE_ID` snapshots the open Edit place and uploads it through Open Cloud.
`--universe` names the destination's experience when it differs from the
project's; `--saved` stores a version without publishing it. Roblox refuses the
upload with HTTP 409 while the destination place is open in a Team Create session;
the error names that session's members and unpublished newer saves when the key
can read them.
`--open-cloud` builds the selected place project, or uploads `--file` unchanged.
Use `ROBLOX_API_KEY` / `--key-env ENV` with Universe Places Write. IDs come from the
experience or explicit flags. Renium rejects instance types the API cannot update
reliably; use Studio publishing for those (see below). `--dry-run` validates the input/target without an
upload or permission check (cloud mode still builds). Temporary builds are cleaned.
Neither mode creates places, publishes packages, or restarts servers. An unconfirmed
response is not success: inspect Version History before retrying.

### Open Cloud limitations and validation

All Open Cloud uploads, including `--as` and `oc place publish`, check size,
parsing and binary float widths. They reject `EditableImage`, `EditableMesh`,
`PartOperation`/`UnionOperation`, `SurfaceAppearance` and `BaseWrap` subclasses;
use `rbx publish` from the destination's Studio session for those. Passing these
checks does not prove engine loading or player joins. See
[validation and API limits](publishing-validation.md) for dry runs and exact scope.

## Source edits and ordered input

```powershell
rbx me src/ServerScriptService/Main.server.luau oldText newText
rbx inp -p 1 click "Shop.BuyButton" wait 100 key E
```

`me FILE OLD NEW [OLD NEW ...]` applies exact replacements.
`--all` replaces every match; `--class` sets a new script's class.

`inp` accepts action/value pairs: `click`, `right`, `move`, `down`, `up`, `right-down`, `right-up`, `scroll-up`, `scroll-down`, `key`, `kd`, `ku`, `text`, `wait`.
Mouse targets are UI paths or `x,y`; waits are milliseconds. Put `-p` first.

## Report a Renium bug

Tell the user about any change you made to their place to get past a Renium failure.

```powershell
rbx report -m "what went wrong"
```

When Renium itself fails (an internal error, a crash, a sync that lost or mangled data), `report` writes `.renium/reports/<ID>/` and a zip beside it: the doctor checks, Studio status, the project file, the daemon log tail, recent crash reports, and the last `rbx` commands with their output from this project's Codex or Claude Code transcripts (`--since MINUTES`, default 120; `--no-transcript` leaves them out). Home paths and values after key, token or password words are masked. Tell the user the ID and path; they review the folder and attach the zip to the GitHub issue link the command prints.

See [Native compatibility](native-compatibility.md) for per-operation probes and the Studio update canary.
