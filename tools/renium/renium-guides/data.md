# Saved project data

Read `RENIUM.md` first.

```powershell
rbx find Workspace -n Door --limit 5
rbx find ServerScriptService -c Script --limit 5
rbx tree Workspace Door --depth 2 --limit 100
rbx inspect Workspace -i editor:id
rbx bg Workspace -i editor:id -p Name
rbx bs Workspace -i editor:id -p DisplayName --str "VIP Man"
rbx bs Workspace -i editor:id -p Transparency --num 0.5
rbx bs Workspace -i editor:id -p Anchored --bool true
rbx bs Workspace -i editor:id -p Reviewed --scope attribute --bool true
rbx ba Workspace -n NewModel -c Model
rbx ba Workspace -I editor:parent -n NewPart -c Part
rbx bss Workspace -i editor:script --str "return 1"
rbx bcl Workspace -i editor:source -I editor:parent
rbx move Workspace -i editor:id -I editor:parent
rbx move StarterGui -i editor:id --to-service ReplicatedStorage -I editor:parent
rbx br Workspace -i editor:id
```

Use `find SERVICE text` for a text search. `-n` is an exact name filter; don't add `*` wildcards.

Property values use `--str`, `--num`, `--bool`, `--null`, or `-j` for another JSON value. `--null` removes the stored override; writing the Roblox default explicitly still stores an override. Automatic writes reject property names missing from the class. Use `--scope property` only for a real newer or hidden Roblox property absent from Renium's bundled schema, and `--scope attribute` to create an attribute.

Set or clear an instance reference with `-j '{"_type":"Ref","settingsId":"editor:target"}'` or `--null`. Ref objects can also use `pathSegments` plus `pathOrdinals` when an ID is unavailable.

Edit an existing project script file directly; don't run `bss` afterward. For a new script, use `ba` to create its Script, LocalScript, or ModuleScript entry, then edit the generated file. `bss --str` or `--source-file` can set its source in the same store operation.

Selectors are `-i` for settings ID, `-x` for index, `-n` for name, `-c` for class, or `--path` with optional `--ords`. Use exactly one selector. Don't combine a path with another selector. Use a service name first; use `-f` only for one explicit store, never both.

Mutation results list only files whose bytes or paths changed in `changedPaths`. An empty list is a successful no-op and needs no push. When a structural edit returns settings IDs, include them with `-i` in the next push along with its `changedPaths`. Don't push an entire changed service settings file without IDs unless you intend to reconcile that whole service.

Batch related reads once. The compact response has one flat result per request in top-level `rs`; it doesn't nest one result inside another. In batch fields, `src` is the source-file path; use `prop:Source` for exact script text.

```powershell
'{"ops":[{"type":"search","q":"Door","limit":5,"fields":"lookup"},{"type":"counts"}]}' | rbx bb Workspace -J -
```

Field presets: `lookup=id,n,c,path`, `tree=id,n,c,cc,ch`, `brief=id,n,c,path,cc`; request one property with `prop:Name` or attribute with `attr:Tags`. Requested properties omitted from a `bb` node aren't serialized overrides; they use the Roblox class default.

Inspect models without importing them:

```powershell
rbx view model.rbxm --json
rbx view model.rbxmx --json
```

Use `--json` when exact script source and stable references are required. Plain model view summarizes source text. `view` accepts `.renium`, `.rbxm`, and `.rbxmx`, not place files; verify place contents from `bep`'s manifest and `sm --stdout` before comparing exported hashes.

RBXM is columnar, so decoding it can materialize a Roblox class default for an instance that never stored that property. Requested `bb` properties also return class defaults. Use `rbx view <store>.renium --json` to distinguish stored overrides before comparing model formats.

Search saved script files without asking Studio to read them again:

```powershell
rbx script-search DataStoreService UpdateAsync --limit 20
rbx script-grep RemoteEvent --limit 100
rbx script-read src/ServerScriptService/Main.server.luau --start-line 40 --end-line 80
```

`script-search` matches files containing every keyword, without case sensitivity, and reports file counts. `script-grep` matches literal source text, is case-sensitive unless `--case-insensitive` is used, and reports line counts. Limits cap returned results while totals still cover the full project.
