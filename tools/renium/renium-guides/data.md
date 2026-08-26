# Saved project data

```powershell
rbx f Workspace -n Door --limit 5
rbx f ServerScriptService -c Script --limit 5
rbx tr Workspace Door --depth 2 --limit 100
rbx in Workspace -i editor:id
rbx bg Workspace -i editor:id -p Name
rbx bs Workspace -i editor:id -p DisplayName --str "VIP Man"
rbx bs Workspace -i editor:id -p Transparency --num 0.5
rbx bs Workspace -i editor:id -p Anchored --bool true
rbx bs Workspace -i editor:id -p Reviewed --scope attribute --bool true
rbx ba Workspace -n NewModel -c Model
rbx ba Workspace -I editor:parent -n NewPart -c Part
rbx bss Workspace -i editor:script --str "return 1"
rbx bcl Workspace -i editor:source -I editor:parent
rbx mv Workspace -i editor:id -I editor:parent
rbx mv StarterGui -i editor:id --to-service ReplicatedStorage -I editor:parent
rbx br Workspace -i editor:id
```

`f SERVICE text` searches text. `-n` matches an exact name; don't add wildcards.

Values use `--str`, `--num`, `--bool`, `--null`, or `-j`. `--null` removes an override; writing the default stores one. Unknown class properties are rejected. Use `--scope property` for a real property missing from Renium's schema and `--scope attribute` for attributes.

Set references with `-j '{"_type":"Ref","settingsId":"editor:target"}'`; clear with `--null`. Without an ID, use `pathSegments` and `pathOrdinals`.

Edit existing script files directly; don't run `bss` afterward. For a new script, create its entry with `ba`, then edit its file. `bss --str` or `--source-file` sets source in one store operation.

Select once with `-i`, `-x`, `-n`, `-c`, or `--path` plus optional `--ords`. Use a service name or `-f` for one store, never both.

`changedPaths` lists real file changes; an empty list is a no-op. Live Sync sends those paths automatically. Without Live Sync, push returned paths with returned settings IDs. Don't push a whole service store without IDs unless full-service reconciliation is intended.

Batch related reads. Results are flat in top-level `rs`. In fields, `src` is the source path; `prop:Source` is exact script text.

```powershell
'{"ops":[{"type":"search","q":"Door","limit":5,"fields":"lookup"},{"type":"counts"}]}' | rbx bb Workspace -J -
'{"ops":[{"type":"counts","id":"editor:folder"},{"type":"search","id":"editor:folder","q":"Door","limit":5,"fields":"lookup"}]}' | rbx bb Workspace -J -
```

Limit `counts` or `search` with `id`, `path`, `index`, `name`, or `className`. Presets: `lookup=id,n,c,path`, `tree=id,n,c,cc,ch`, `brief=id,n,c,path,cc`. Request fields with `prop:Name` or `attr:Tags`. A requested property absent from a node uses the class default.

Inspect models without importing them:

```powershell
rbx v model.rbxm --json
rbx v model.rbxmx --json
```

Use `--json` for exact source and references. `v` accepts `.renium`, `.rbxm`, and `.rbxmx`, not places. Verify places with `bep`'s manifest and `sm --stdout`.

RBXM and requested `bb` properties may materialize class defaults. Use `rbx v <store>.renium --json` to identify stored overrides.

Search saved script files without asking Studio to read them again:

```powershell
rbx ss DataStoreService UpdateAsync --limit 20
rbx sg RemoteEvent --limit 100
```

`ss` finds files containing every keyword, case-insensitively. `sg` finds literal lines and is case-sensitive unless changed. Limits cap results, not totals.

Read known scripts as normal files. Use `sr` only for bounded reads when direct access is unavailable.
