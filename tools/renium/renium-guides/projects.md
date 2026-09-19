# Models, places, packages, and Git

## Files

```powershell
rbx bem Workspace -i editor:id -o model.rbxm
rbx bim Workspace --model model.rbxm --parent-settings-id editor:parent
rbx bep -o place.rbxl
rbx pi Place.rbxl
rbx q Place.rbxl -n RewardHandler
rbx q Place.rbxl --source "reward granted"
rbx cmp Place.rbxl
rbx cmp Before.rbxl --full --all
rbx cmp Before.rbxl --against After.rbxlx --full --all
rbx sm
rbx sm --stdout
rbx sm --cached --stdout --filter "*Tutorial*"
rbx bpack
```

`bem`/`bim` copy model trees; use `mv --to-service` for an existing subtree.
`bep` builds a place without opening or publishing it; `bep --base ORIGINAL.rbxl` keeps that file's unsynced services and engine root fields so the result is a complete place. `pi` does the reverse, importing a saved `.rbxl`/`.rbxlx` into the project files like a pull, without Studio. Once Studio has that place open, run `pl` once before Live Sync or pushes so the files adopt Studio's instance identities.
`q` queries an RBXL/RBXLX without Studio or a project beside it. Filter by name, class, or source.
To pull or import into a separate folder, pass `-r DIR` (`rbx pi FILE -r DIR`, or `rbx -r DIR pi FILE` to run any command from that folder); an empty folder gets its project file created. Create projects with `rbx init DIR`, never by writing `renium.project.jsonc` by hand.
`cmp` is script-only by default. Add `--full --all` for all instance/property/attribute changes; `--values` includes values and source. The input is the older/before state; the project (or `--against` file) is after. `.rbxl` and `.rbxlx` work on either side without opening Studio. See [comparison scope and output](data.md#inspect-files-without-importing).

`sm` generates the sourcemap; `--cached` queries it. `bpack` upgrades old stores.

## Roblox packages

```powershell
rbx pd ReplicatedStorage.SharedPackage
rbx pp ReplicatedStorage.SharedPackage
rbx pu ReplicatedStorage.SharedPackage
rbx upl ReplicatedStorage --settings-id editor:package
```

Files → Studio edits mark affected linked packages **Changed** before modifying descendants. The result's `autoDesyncedPackages` lists them; report those paths. Failed edits also report packages already marked Changed. Their PackageLinks stay intact.

- `pd`: mark Changed without editing contents.
- `pp`: publish changes; requires user authorization.
- `pu`: discard changes and fetch the latest published version.
- `upl`: remove the PackageLink while keeping contents. This is unlinking, not desync.

On Windows/macOS, `pd`/`pp`/`pu` target the package root without selection, dialogs, or focus. Their operation budget is 20 seconds, not a guarantee that Roblox publishing always succeeds.
Use a JSON string array for names containing dots, `Name[2]` or `--ords` for duplicates, and `--pid PID` only when several processes match.

## Wally

```powershell
rbx wally --realms shared
rbx wally --realms shared --force
```

The project must provide `wally`; Aftman projects must declare it.
`--force` reinstalls current packages; `--details` adds full paths/IDs.

## Reusable local/Git links

```powershell
rbx lka --id logger --source-type local --source links/Logger.luau --service ReplicatedStorage --path '["ReplicatedStorage","Shared","Logger"]'
rbx lk
rbx lks
rbx lkb --service ReplicatedStorage --path '["ReplicatedStorage","Shared","Logger"]' --remove
```

`lka` adds a target, `lk` applies it, `lks` reports status, and `lkb` detaches it.
`--remove` also deletes its record, not the editable target.
Sources are project-relative; links are read-only unless `--writable`.
Live Sync sends changed paths; otherwise push only when Studio needs them.

Git sources use `--source-type git --source REPOSITORY --ref REF --subpath PATH`.
`lk` refreshes the cached ref; `--offline` requires a cache.

```powershell
rbx lkp --link-folder packages --id shared-widget --service ReplicatedStorage --path '["ReplicatedStorage","PackageSource"]'
rbx lka --id shared-widget --service ReplicatedStorage --path '["ReplicatedStorage","PackageCopy"]'
rbx lk --link shared-widget
rbx lkd --id shared-widget --action unlink-uses
```

`lkp` packages a subtree and registers its source; reuse its ID with `lka`.
`lkd` deletes a package/link: `delete-unused` refuses active uses, `delete-uses` removes them, and `unlink-uses` keeps editable copies.

## Version control

Run `rbx vci` to install Git ignore/diff/merge rules; reruns are safe.
`vct` renders stores as text; Git calls `vcm` for store merge conflicts.
Avoid `--untracked-files=all` on generated packages. Git tracks files; a commit or push does not publish Roblox content.

## Live collaboration

```powershell
rbx collab start
rbx collab relay deploy
rbx collab start --relay
rbx collab join wss://host.trycloudflare.com/?token=abc -r C:/proj
rbx collab status
rbx collab invite
rbx collab stop
```

`start` shares the project's files as one live document and prints an invite link; every participant's folder mirrors it. Without `--relay`, the room runs on the host through a Cloudflare quick tunnel, so the invite dies when the host stops. With `--relay`, a relay keeps the room and its history, and the relay's copy wins over any local folder on join. `relay deploy` publishes the relay to the user's free Cloudflare account once (Node.js required; a browser sign-in may open) and makes it the default; `relay set URL` picks an existing one.
`join` fills an empty folder from the room; an existing folder is overwritten to match. Only one participant, the host, keeps Live Sync with Studio; others edit files and see the result through Team Create or the host.
`status` lists participants with their open file and selection. Do not start a second session for the same folder; stop the first.
