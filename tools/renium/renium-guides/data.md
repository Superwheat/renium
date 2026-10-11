# Saved data

These commands read project files, not Studio. Select the Roblox service that owns
the target. Scripts live under the configured source folders; instance stores live
in `instances/` beside `src`.

## Find and read

```powershell
rbx f Workspace -n Door --limit 5
rbx f ServerScriptService -c Script --limit 5
rbx f ReplicatedStorage --path Vehicles.Cars -c Model
rbx tr Workspace Door --depth 2 --limit 100
rbx in Workspace -i editor:id
rbx bg Workspace Lobby.Door Anchored
rbx bg Workspace -i editor:id Name
rbx bg Workspace -p Gravity
rbx ss DataStoreService UpdateAsync --limit 20
rbx sg RemoteEvent --limit 100
```

`f SERVICE text` searches text; `-n` matches an exact name, without wildcards; `--path` limits the search to one subtree.
`ss` finds scripts containing every keyword, case-insensitively. `sg` finds literal lines, case-sensitive by default. Limits cap returned results, not totals. Read known scripts directly; use `sr` for bounded reads when file access is unavailable.

Use one lookup on the relevant state: `f` for saved data, `q` for a closed place, `l` for unsaved Studio state. Refine ambiguous results instead of repeating the query through different tools. Git comparisons are useful when the question concerns revisions, not as an extra existence check.

## Edit

```powershell
rbx bs Workspace -i editor:id -p Name --str "VIP Man"
rbx bs Workspace Lobby.Door -p Transparency --num 0.5
rbx bs Workspace -p Gravity --num 120
rbx bs Workspace -i editor:id -p Reviewed --scope attribute --bool true
rbx ba Workspace -n NewModel -c Model
rbx ba Workspace -I Lobby.Props -n NewPart -c Part
rbx bss Workspace -i editor:script --str "return 1"
rbx bcl Workspace -i editor:source -I editor:parent
rbx mv Workspace -i editor:id -I editor:parent
rbx mv StarterGui -i editor:id --to-service ReplicatedStorage -I editor:parent
rbx br Workspace Lobby.OldPart
```

Every store command takes the target the same way: a positional name or dotted path (`Lobby.Door`), `-i ID`, `-x INDEX`, `-n NAME` or `-c CLASS`. `bg` takes the property after the target or as `-p`; `bs` takes `-p`. `bg`/`bs` with no target address the service itself. Parent options (`-I`) take an ID or a dotted path. Duplicates take inline ordinals (`Borders.Border[4]`, the form compact output prints) or `--ords`.
Use a service name or `-f STORE`, not both.

Values: `--str`, `--num`, `--bool`, `--null`, or `-j JSON`. Use `-j -` to read JSON from stdin, including values too large for the OS command line.
`--null` removes an override; writing the default stores one.
Unknown properties are rejected. `--scope property` explicitly selects a real property absent from the schema; `--scope attribute` selects an attribute.

References use `-j '{"_type":"Ref","settingsId":"editor:target"}'`; clear with `--null`.
Without an ID, use `pathSegments` and `pathOrdinals`.

Edit existing script files directly; no follow-up `bss` is needed.
For a new script, create its entry with `ba`, then edit the file. `bss --str` or `--source-file` writes source through the store.

Check changed scripts together with `rbx ck`, which takes the `.luau`/`.lua` files git reports as changed in the project; name files or folders (`rbx ck src/ServerScriptService`) otherwise. It parses Luau offline without executing it; use `rbx ck -` for UTF-8 source on stdin. Errors include the file and syntax location, and return a failing exit code. This is not type checking, lint, or a behavior test.

`changedPaths` lists actual file changes; empty means no-op. Live Sync sends them automatically.
Without Live Sync, push returned paths with their settings IDs, not an entire service.

## Bulk and protected properties

See [Bulk and protected properties](data-properties.md).

## Inspect saved files

See [Inspect saved files](data-inspect.md).
