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

`bem`/`bim` copy model trees; use `mv --to-service` for an existing subtree. `bim` and script cloning refuse to overwrite a different source file already at the destination; identical source is reused.
`bep` builds a place without Studio; `--base ORIGINAL.rbxl` keeps its unsynced services and root fields (HttpService, AvatarSettings, PhysicsService, ...). `pi` records the imported place, and `bep`/`build` use it as the base while it is unchanged on disk (`baseSource: import`); the result carries a `warning` when it cannot. Model exports and native insertions also avoid mixed collision-hull columns. The base supplies engine geometry the files lack: MeshPart collision hulls, union meshes, Terrain.Materials. A class with any part whose hull is unknown (new or imported parts) is written without hulls so Studio rebuilds them on load; the build logs it, and `rbx q` lists such values under `defaulted`. Stores imported before 2026-10-11 lack this data; re-import their places with `pi` before `bem` moves parts elsewhere. `pi` imports a saved `.rbxl`/`.rbxlx` into the files, saved service fields the plugin cannot read included; once Studio opens that place, run `pl` once before Live Sync or pushes so files adopt Studio's ids.
`q` searches a place file by name, class, or source without Studio. `--props A,B` (or `all`) adds saved values, NotScriptable ones included; enums print as `Enum.Type.Item`, references as paths. `defaulted` names were not in the file (class default shown), `notSaved` ones are derived (Position), `unknown` ones are not properties.
To pull or import into another folder, pass `-r DIR` (`rbx -r DIR pi FILE`); an empty folder gets a project file. Create projects with `rbx init DIR`, never by writing `renium.project.jsonc` by hand.
`cmp` compares scripts; `--full --all` adds instance/property/attribute changes, `--values` values and source. The input is before, the project (or a second file) after. See [comparison scope and output](data-inspect.md#inspect-files-without-importing).
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

## Links, version control, and collaboration

See [Links, version control, and collaboration](links.md).
