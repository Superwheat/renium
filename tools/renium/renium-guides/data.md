# Saved data

These commands read project files, not Studio. Select the Roblox service that owns
the target. Scripts live under the configured source folders; instance stores live
in `instances/` beside `src`.

## Find and read

```powershell
rbx f Workspace -n Door --limit 5
rbx f ServerScriptService -c Script --limit 5
rbx tr Workspace Door --depth 2 --limit 100
rbx in Workspace -i editor:id
rbx bg Workspace -i editor:id -p Name
rbx ss DataStoreService UpdateAsync --limit 20
rbx sg RemoteEvent --limit 100
```

`f SERVICE text` searches text; `-n` matches an exact name, without wildcards.
`ss` finds scripts containing every keyword, case-insensitively. `sg` finds literal lines, case-sensitive by default. Limits cap returned results, not totals. Read known scripts directly; use `sr` for bounded reads when file access is unavailable.

Use one lookup on the relevant state: `f` for saved data, `q` for a closed place, `l` for unsaved Studio state. Refine ambiguous results instead of repeating the query through different tools. Git comparisons are useful when the question concerns revisions, not as an extra existence check.

## Edit

```powershell
rbx bs Workspace -i editor:id -p Name --str "VIP Man"
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

Select with `-i`, `-x`, `-n`, `-c`, or `--path` plus `--ords` for duplicates.
Use a service name or `-f STORE`, not both.

Values: `--str`, `--num`, `--bool`, `--null`, or `-j JSON`. Use `-j -` to read JSON from stdin, including values too large for the OS command line.
`--null` removes an override; writing the default stores one.
Unknown properties are rejected. `--scope property` explicitly selects a real property absent from the schema; `--scope attribute` selects an attribute.

References use `-j '{"_type":"Ref","settingsId":"editor:target"}'`; clear with `--null`.
Without an ID, use `pathSegments` and `pathOrdinals`.

Edit existing script files directly; no follow-up `bss` is needed.
For a new script, create its entry with `ba`, then edit the file. `bss --str` or `--source-file` writes source through the store.

Check changed scripts together with `rbx ck src/ReplicatedStorage/Config.luau src/ServerScriptService/Main.server.luau`. It parses Luau offline without executing it; use `rbx ck -` for UTF-8 source on stdin. Errors include the file and syntax location, and return a failing exit code. This is not type checking, lint, or a behavior test.

`changedPaths` lists actual file changes; empty means no-op. Live Sync sends them automatically.
Without Live Sync, push returned paths with their settings IDs, not an entire service.

## Bulk reads

Request the needed fields once and analyze locally; don't make Studio scan saved data.

```powershell
'{"ops":[{"type":"search","q":"Door","limit":5,"fields":"lookup,prop:Anchored"},{"type":"counts"}]}' | rbx bb Workspace -J -
'{"ops":[{"type":"counts","id":"editor:folder"},{"type":"search","id":"editor:folder","q":"Door","limit":5,"fields":"lookup"}]}' | rbx bb Workspace -J -
```

Results are flat in `rs`. Scope `search`/`counts` by `id`, `path`, `index`, `name`, or `className`.
Field presets: `lookup=id,n,c,path`, `tree=id,n,c,cc,ch`, `brief=id,n,c,path,cc`.
Use `prop:Name`/`attr:Tags` for fields; `src` is a source path, `prop:Source` is exact text.
Missing requested properties use class defaults.

## Protected Studio properties

Use ordinary file edits and queries first. `access` is for live engine properties that normal APIs cannot access, not a replacement for saved-data workflows.

```powershell
rbx access read Workspace StreamingEnabled
rbx access approve REQUEST_ID
rbx access write Workspace.Mesh CollisionFidelity Hull
rbx access mode read-only
rbx access mode ask
```

The default `ask` mode returns an exact `approval-required` request for unlisted properties. Inspect its target, property and write value before approving; approvals expire, are single-use, and cannot transfer to replacement instances or sessions. `reject REQUEST_ID` discards one. CollisionFidelity is allowlisted with validated enum values.

`read-only` allows protected reads and rejects protected writes. `read-write` allows both, but requires the user's explicit request, a warning about unknown scripts/plugins, and `--accept-risk`. Modes apply only to the selected runtime; they do not restrict ordinary edits or Live Sync. Built-in performance diagnostics remain trusted operations.

Values use Studio's text representation, up to 64 KiB. Writes verify the resulting value and mark affected packages Changed before editing; report `autoDesyncedPackages`. If an asynchronous write times out, read its current value before retrying—it may still finish. Unsupported codecs or setters return an error, not a guessed memory edit. Native property calls support Windows and macOS Edit mode, not play clients.

After a Studio update, Renium automatically rediscovers and validates the native entry points, then caches them for that executable. An update alone does not disable access. If the new layout cannot be validated, the error identifies the detector that needs updating; do not force an old address or repeatedly retry the same failure.

These commands are authenticated and do not expose a privileged Luau function or weaken global Studio permissions. Arbitrary code already running as the same OS user can invoke the CLI; do not claim protection against a compromised user account.

## Inspect files without importing

```powershell
rbx v model.rbxm --json
rbx v model.rbxmx --json
rbx v Place.rbxl --json
rbx v Place.rbxlx --json
rbx q Place.rbxl -n Door
rbx q Place.rbxl --source "reward granted"
rbx cmp Place.rbxl
rbx cmp Before.rbxl --full --all
rbx cmp Before.rbxl --against After.rbxlx --full --all
```

`v` accepts `.rbxl`/`.rbxlx` places, `.rbxm`/`.rbxmx` models and `.renium` stores; `--json` includes source, properties, attributes and references. It inspects one file; it does not compare two states.
Use `v STORE.renium --json` to distinguish stored overrides from materialized defaults.
For a full saved-place diff, use `cmp BEFORE --full` from the target project. Add `--against AFTER` for two files; no project is required. Both place formats work in either position. The direction is BEFORE → project/AFTER. Without `--full`, `cmp` is script-only.

The full report lists added/removed instances and changed property/attribute names, including Source, references, packages and serialized Terrain data. `--values` includes before/after values and source; these may contain credentials, so redact them before sharing. `--limit` caps entries, not counts; `--all` returns every difference.

Comparison ignores file-local IDs/history, sibling and tag order, line endings, and explicitly stored class defaults. References compare by matched targets; duplicate subtrees use Renium's identity matcher. Without persistent identity, a rename, move or class replacement can appear as removal/addition. Only project services are compared against a project; two files compare their union of services. `notSerializedProjectProperties` counts project properties Roblox does not save as standalone fields (such as CollisionFidelity); these cannot be checked directly, but their saved data is still compared. This compares decoded saved content, not published assets, DataStores or runtime state.

For current Studio versus an old file, use the synchronized project. If a known sync failure/pending edit makes it stale, resolve that and use one `lst --wait`; don't open the old file, import it, start Play or dump both trees just to assemble a diff.
