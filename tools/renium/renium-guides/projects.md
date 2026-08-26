# Models, places, links, packages, and version control

Also read `RENIUM/data.md` before editing saved instances.

```powershell
rbx bem Workspace -i editor:id -o model.rbxm
rbx bim Workspace --model model.rbxm --parent-settings-id editor:parent
rbx bep -o place.rbxl
rbx x -d snapshots --no-run-import
rbx si --snapshot-dir snapshots --project-root .
rbx sm
rbx sm --stdout
rbx sm --cached --stdout --filter "*Tutorial*"
rbx bpack
rbx wally --realms shared
rbx wally --realms shared --force
```

Wally sync needs `wally`; Aftman projects must declare it. `--force` reinstalls current packages. `--details` includes full path and ID lists.

`bem`/`bim` copy model trees. Use `mv --to-service` to move an existing subtree across services. `x` exports Studio snapshots; `si` imports them. Both snapshot export and pull need the same bridge. `bep` builds a place. `sm` maps every instance; `--cached --stdout --filter GLOB` queries the existing map. `bpack` updates old stores only.

Run `rbx vci` once to set up Git ignore, diff, and merge rules; reruns are safe. `vct` renders a store as text. Git calls `vcm` for conflicting `.renium` merges. Avoid `--untracked-files=all` on generated packages.

Mirror one local source into a project target:

```powershell
rbx lka --id logger --source-type local --source links/Logger.luau --service ReplicatedStorage --path '["ReplicatedStorage","Shared","Logger"]'
rbx lk
rbx lks
rbx lkb --service ReplicatedStorage --path '["ReplicatedStorage","Shared","Logger"]' --remove
```

`lka` adds a target, `lk` applies it, and `lks` reports status. `lkb` detaches a target; `--remove` also deletes its record. Detached targets remain editable. Local sources are project-relative. Links are read-only unless `--writable`. Live Sync sends returned paths; otherwise push only when Studio needs the update.

For Git, use `--source-type git --source REPOSITORY --ref REF --subpath PATH`. `lk` refreshes the cached ref; `--offline` requires an existing cache.

Pack an existing subtree into a reusable project package, insert it elsewhere, then remove the package while keeping both materialized trees:

```powershell
rbx lkp --link-folder packages --id shared-widget --service ReplicatedStorage --path '["ReplicatedStorage","PackageSource"]'
rbx lka --id shared-widget --service ReplicatedStorage --path '["ReplicatedStorage","PackageCopy"]'
rbx lk --link shared-widget
rbx lkd --id shared-widget --action unlink-uses
```

`lkp` writes the package and registers its source subtree. Reuse its ID with `lka`. `lkd` can refuse active uses (`delete-unused`), remove them (`delete-uses`), or keep editable copies (`unlink-uses`). Each action deletes the package and link.
