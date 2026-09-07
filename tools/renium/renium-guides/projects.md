# Models, places, packages, and Git

## Files

```powershell
rbx bem Workspace -i editor:id -o model.rbxm
rbx bim Workspace --model model.rbxm --parent-settings-id editor:parent
rbx bep -o place.rbxl
rbx q Place.rbxl -n RewardHandler
rbx q Place.rbxl --source "reward granted"
rbx cmp Place.rbxl
rbx cmp Before.rbxl --full --all
rbx cmp Before.rbxl --against After.rbxlx --full --all
rbx x -d snapshots --no-run-import
rbx si --snapshot-dir snapshots --project-root .
rbx sm
rbx sm --stdout
rbx sm --cached --stdout --filter "*Tutorial*"
rbx bpack
```

`bem`/`bim` copy model trees; use `mv --to-service` for an existing subtree.
`bep` builds a place without opening or publishing it.
`q` queries an RBXL/RBXLX without Studio or a project beside it. Filter by name, class, or source.
`cmp` is script-only by default. Add `--full --all` for all instance/property/attribute changes; `--values` includes values and source. The input is the older/before state; the project (or `--against` file) is after. `.rbxl` and `.rbxlx` work on either side without opening Studio. See [comparison scope and output](data.md#inspect-files-without-importing).

`x` exports Studio snapshots; `si` imports them. Snapshot export and pull use the same bridge; don't use snapshots for a simple closed-file query.
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
Use a JSON string array for names containing dots, `--ords` for duplicates, and `--pid PID` only when several processes match.

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
