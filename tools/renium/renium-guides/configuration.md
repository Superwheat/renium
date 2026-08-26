# Project configuration and adapters

## Settings

```powershell
rbx cfg list
rbx cfg get liveSync.initialSyncPriority
rbx cfg set liveSync.initialSyncPriority reconcile
```

`list` shows every setting, its current value, and valid values. `set` writes the active place; add `--scope user`, `workspace`, or `experience` only when that wider scope is intended.

```powershell
rbx fmt --check
rbx fmt
rbx xp data/config.json
rbx pv
rbx ad build
rbx ad build --check
rbx ad syncback --preview
rbx ad syncback
rbx ir --project default.project.json --preview
rbx ir --project default.project.json --apply
rbx ip .\Shared.server.luau --path-json '["ServerScriptService","Shared"]' --dry-run
rbx ip .\Shared.server.luau --path-json '["ServerScriptService","Shared"]'
rbx ip .\SharedFolder --destination src\ReplicatedStorage\Shared --dry-run
```

`build` maps source files to instances. `syncback` writes supported instance edits to adapter sources. Use `--check` or `--preview` for validation; don't use `watch` for one-off work.

`pv` validates the project offline. `ir` converts one Rojo project into `renium.project.jsonc`; preview first.

`ip` copies a file by Roblox path or a directory by project path. Preview first; omitting `--dry-run` applies the listed actions.

Mounts use `{"source":"shared","target":"ReplicatedStorage.Shared","ownership":"read-only","optional":true}`. Ownership defaults to `exclusive`; optional missing sources project nothing. Reads include mounts, `bss` edits writable mounted scripts, and `xp` follows nested projects.

`syncRules` map extra file types. Last match wins; `suffix` strips a suffix, `exclude` rejects a rule, and `use: "ignore"` suppresses a file. `globIgnorePaths` blocks paths before projection.

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

Filters use `include` or `ignore` in `files-to-studio`, `studio-to-files`, or `both`. Selectors: `glob`, `name`, `class`, `tag`, `attribute`, `property`, `id`. Last match wins; field selectors affect only that field. In `xp`, `owned` means mapped, `ignored` means blocked, and `selectedSyncRule` is the winning rule.
