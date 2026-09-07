# Configuration and adapters

## Settings and validation

```powershell
rbx cfg list
rbx cfg get liveSync.initialSyncPriority
rbx cfg set liveSync.initialSyncPriority reconcile
rbx fmt --check
rbx fmt
rbx pv
rbx xp data/config.json
```

`cfg list` includes current and allowed values. Writes affect the active place; use `--scope user`, `workspace`, or `experience` only for wider changes.
`pv` validates project configuration and mappings offline, not script syntax; use `ck` for Luau syntax. `xp` explains how a path maps to instances, including nested projects.

## Adapters and imports

```powershell
rbx ad build --check
rbx ad build
rbx ad syncback --preview
rbx ad syncback
rbx ir --project default.project.json --preview
rbx ir --project default.project.json --apply
rbx ip ./Shared.server.luau --path-json '["ServerScriptService","Shared"]' --dry-run
rbx ip ./Shared.server.luau --path-json '["ServerScriptService","Shared"]'
rbx ip ./SharedFolder --destination src/ReplicatedStorage/Shared --dry-run
```

`ad build` maps source files to instances; `syncback` writes supported instance edits to adapter sources. Use checks/previews for validation, not a persistent watcher.
`ir` converts one Rojo project to `renium.project.jsonc`.
`ip` copies a file by Roblox path or a directory by project path. Preview imports first; dropping the preview flag applies them.

## Mounts and rules

Mount example: `{"source":"shared","target":"ReplicatedStorage.Shared","ownership":"read-only","optional":true}`.
Ownership defaults to `exclusive`; optional missing sources project nothing. Reads include mounts; `bss` supports writable mounted scripts.

```jsonc
{
  "syncRules": [
    { "pattern": "**/*.server.txt", "use": "serverScript", "suffix": ".server.txt" },
    { "pattern": "**/draft/**", "use": "ignore" }
  ],
  "globIgnorePaths": ["src/generated/**"],
  "filters": [
    { "action": "ignore", "direction": "files-to-studio", "class": "ModuleScript" },
    { "action": "include", "direction": "files-to-studio", "name": "Shared" },
    { "action": "ignore", "direction": "both", "glob": "Workspace/Generated/**", "property": "Source" }
  ]
}
```

`syncRules` map extra file types: last match wins, `suffix` strips a suffix, `exclude` rejects a rule, `use: "ignore"` suppresses a file. `globIgnorePaths` blocks paths before projection.

Filters apply `include`/`ignore` in `files-to-studio`, `studio-to-files`, or `both`.
Selectors: `glob`, `name`, `class`, `tag`, `attribute`, `property`, `id`.
Last match wins; field selectors affect only that field.
In `xp`, `owned` means mapped, `ignored` means blocked, and `selectedSyncRule` identifies the winning rule.
