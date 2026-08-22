# Models, places, links, packages, and version control

Read `RENIUM.md` first. Read `RENIUM/data.md` too before selecting or editing saved instances.

```powershell
rbx bem Workspace -i editor:id -o model.rbxm
rbx bim Workspace --model model.rbxm --parent-settings-id editor:parent
rbx bep -o place.rbxl
rbx x -d snapshots --no-run-import
rbx si --snapshot-dir snapshots --project-root .
rbx sm
rbx sm --stdout
rbx bpack
rbx wally --realms shared
rbx wally --realms shared --force
```

Wally sync needs a working `wally` command; Aftman users must declare Wally for the project first. Use `--force` to reinstall and reimport packages that are already current. Add `--details` only when the full changed-path and instance-ID lists are needed.

`bem`/`bim` export and import model trees. They copy instances; use `mv --to-service` to reparent an existing project subtree across services without a temporary model or a separate remove. `x` exports raw Studio snapshots; `si` imports them into project files. `bep` builds a place from project data. `sm` writes the sourcemap for all mapped instances, not only scripts; use `sm --stdout` when its contents are needed without creating a file. `bpack` rewrites project stores in the current format and reports which files changed; already-current files remain untouched.

Version control: run `rbx vci` once in a project to initialize Git and Renium's ignore, text-diff, and merge rules; rerunning it is safe. `rbx vct FILE.renium` renders one binary store as deterministic text. Git invokes `rbx vcm BASE OURS THEIRS` automatically for a conflicting `.renium` merge. Use normal or path-scoped `git status`; `--untracked-files=all` expands every generated package file.

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
