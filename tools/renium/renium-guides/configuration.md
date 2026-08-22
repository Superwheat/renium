# Project configuration and adapters

Read `RENIUM.md` first.

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

`build` maps configured source files into project instances. `syncback` writes supported two-way instance edits back to their adapter sources. Use `--check` or `--preview` before a write when only validation or a plan is needed; don't use `watch` for one-off agent work.

`pv` checks the complete project offline. `ir` converts one Rojo project file or a folder containing exactly one into a formatted `renium.project.jsonc`; preview before applying.

`ip` copies one file by Roblox path or a directory by project-relative destination. Preview first; existing files are reported as `unchanged` or `overwrite`, and omitting `--dry-run` applies the listed actions.

Mounts use `{"source":"shared","target":"ReplicatedStorage.Shared","ownership":"read-only","optional":true}` in the project's `mounts` array. Ownership defaults to `exclusive`; optional missing sources project nothing. Normal reads include mounted instances, `bss` writes writable mounted scripts, and `xp` follows nested-project descendants.

`syncRules` map extra file types into instances. Rules are ordered and the last match wins; `suffix` strips a file suffix, `exclude` disqualifies that rule, and `use: "ignore"` suppresses the file. `globIgnorePaths` ignores matching project paths before projection.

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

Filter actions are `include` or `ignore`; directions are `files-to-studio`, `studio-to-files`, or `both`. Selectors are `glob`, `name`, `class`, `tag`, `attribute`, `property`, and `id`. Filters are ordered and last-match wins; `property` and `attribute` rules affect only that field. In `xp`, `owned` means a mapping claims the path, `ignored` means `globIgnorePaths` blocks it, and `selectedSyncRule` identifies the winning rule even when another setting suppresses its output.
