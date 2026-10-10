# Models, places, packages, and Git

## Files

```powershell
rbx bem Workspace -i editor:id -o model.rbxm
rbx bim Workspace --model model.rbxm --parent-settings-id editor:parent
rbx bep -o place.rbxl
rbx pi Place.rbxl
rbx q Place.rbxl -n RewardHandler
rbx q Place.rbxl -c Workspace --props PlayerCharacterDestroyBehavior,Gravity
rbx cmp Before.rbxl After.rbxlx --full --all
```

`bem`/`bim` copy model trees; use `mv --to-service` for an existing subtree.
`bep` builds a place without Studio; `--base ORIGINAL.rbxl` keeps its unsynced services and root fields (HttpService, AvatarSettings, PhysicsService, ...). `pi` records the imported place, and `bep`/`build` use it as the base while it is unchanged on disk (`baseSource: import`); the result carries a `warning` when it cannot. The base also supplies MeshPart collision hulls the files lack (a part whose hull is unknown leaves the whole class for Studio to rebuild on load; the build logs it). `pi` imports a saved `.rbxl`/`.rbxlx` into the files, saved service fields the plugin cannot read included; once Studio opens that place, run `pl` once before Live Sync or pushes so files adopt Studio's ids.
`q` searches a place file by name, class, or source without Studio. `--props A,B` (or `all`) adds saved values, NotScriptable ones included; enums print as `Enum.Type.Item`, references as paths. `defaulted` names were not in the file (class default shown), `notSaved` ones are derived (Position), `unknown` ones are not properties.
To pull or import into another folder, pass `-r DIR` (`rbx -r DIR pi FILE`); an empty folder gets a project file. Create projects with `rbx init DIR`, never by writing `renium.project.jsonc` by hand.
`cmp` compares scripts; `--full --all` adds instance/property/attribute changes, `--values` values and source. The input is before, the project (or a second file) after. See [comparison scope and output](data.md#inspect-files-without-importing).
`sm` writes the sourcemap and `sm --cached --stdout --filter GLOB` queries it; `bpack` upgrades old stores.

## Roblox packages

```powershell
rbx pd ReplicatedStorage.SharedPackage
rbx pp ReplicatedStorage.SharedPackage
rbx pu ReplicatedStorage.SharedPackage
rbx upl ReplicatedStorage --settings-id editor:package
```

Files → Studio edits mark affected linked packages **Changed** before editing descendants and list them in `autoDesyncedPackages`; report those paths. Failed edits also report packages already marked Changed. PackageLinks stay intact.

- `pd`: mark Changed without editing contents.
- `pp`: publish changes; requires user authorization.
- `pu`: discard changes and fetch the latest published version.
- `upl`: remove the PackageLink while keeping contents (unlinking, not desync). With the place open in Studio it unlinks there and Live Sync pulls the files; with Studio closed it edits the files only, and a later push of that edit is refused, so open the place first.

On Windows/macOS, `pd`/`pp`/`pu` target the package root without selection, dialogs, or focus and wait up to two minutes (`--timeout` up to 600 s); a finished wait does not prove publishing succeeded.
Use a JSON string array for names containing dots, `Name[2]` or `--ords` for duplicates, and `--pid PID` only when several processes match.

## Wally

```powershell
rbx wally --realms shared
```

The project must provide `wally`; Aftman projects must declare it. `--force` reinstalls current packages; `--details` adds full paths/IDs.

## Reusable local/Git links

```powershell
rbx lka --id logger --source links/Logger.luau --service ReplicatedStorage --path '["ReplicatedStorage","Shared","Logger"]'
rbx lk
rbx lks
rbx lkb --service ReplicatedStorage --path '["ReplicatedStorage","Shared","Logger"]' --remove
```

`lka` adds a target, `lk` applies it, `lks` reports status, `lkb` detaches it (`--remove` also drops its record). Sources are project-relative; links are read-only unless `--writable`. Live Sync sends changed paths; otherwise push only when Studio needs them.
Git sources use `--source-type git --source REPOSITORY --ref REF --subpath PATH`; `lk` refreshes the cached ref, `--offline` requires a cache.
`lkp --link-folder packages --id ID` packages a subtree (same `--service`/`--path`) and registers its source; reuse the ID with `lka`. `lkd --id ID --action` deletes a package: `delete-unused` refuses active uses, `delete-uses` removes them, `unlink-uses` keeps editable copies.

### Shared links across places

```powershell
rbx --place lobby lkp --share --service ReplicatedStorage --path '["ReplicatedStorage","Public"]'
rbx lk --experience
rbx lks --experience
```

`lkp --share` in the source place packs `links/ID.renium` at the experience root, records it in `renium.experience.json` (`sharedLinks`) and links each place with the same path read-only (`--all-places`: every place). Edit only the source, then `rbx lk` in any place (`--experience` from anywhere) re-packs and updates every copy; Live Sync does this itself and pushes each copy. Copies hold read-only script files; other files under their root move to `.renium/link-replaced/ID/`.
A PackageLink right under the root is left out (`strippedPackageLinks`) and dropped from the copies; `rbx upl` unlinks it in the source. Nested packages travel as they are: pushes into packages the account does not own are refused before Studio is touched, so copies get those only unchanged. `lks --experience` shows `stale` packs and places as `ok`, `drift` or `missing`.

## Version control

Run `rbx vci` to install Git ignore/diff/merge rules; reruns are safe. `vct` renders stores as text; Git calls `vcm` for store merge conflicts.
Avoid `--untracked-files=all` on generated packages. Git tracks files; a commit or push does not publish Roblox content.

## Live collaboration

```powershell
rbx collab start --relay
rbx collab relay deploy
rbx collab join wss://host.trycloudflare.com/?token=abc -r C:/proj
rbx collab status
rbx collab invite
rbx collab stop
```

`start` shares the project's files as one live document and prints an invite; every participant's folder mirrors it. Without `--relay` the room runs on the host through a Cloudflare quick tunnel and dies with it; with `--relay` a relay keeps the room and its history, and its copy wins over a local folder on join. `relay deploy` publishes the relay to the user's free Cloudflare account once (needs Node.js; may open a browser sign-in) and makes it the default; `relay set URL` picks an existing one.
`join` fills an empty folder from the room and overwrites an existing one. Only the host runs Live Sync; others edit files and see results through Team Create or the host.
`status` lists participants, their open file and selection. Never start a second session for one folder; stop the first.
