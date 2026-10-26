# Inspect saved files

See [data](data.md) for the main workflow.

## Inspect files without importing

```powershell
rbx v model.rbxm --json
rbx v model.rbxmx --json
rbx v Place.rbxl --json
rbx v Place.rbxlx --json
rbx q Place.rbxl -n Door
rbx q Place.rbxl --source "reward granted"
rbx q Place.rbxl -c Workspace --props StreamingEnabled
rbx cmp Place.rbxl
rbx cmp Before.rbxl --full
rbx cmp Before.rbxl After.rbxlx --full
```

`v` accepts `.rbxl`/`.rbxlx` places, `.rbxm`/`.rbxmx` models and `.renium` stores; `--json` includes source, properties, attributes and references. It inspects one file; it does not compare two states. For a few saved values, use `q ... --props NAME,NAME` instead of a whole `v --json` dump. `q` prints `matches` as `{path, className, props}` with a dotted path (`Workspace.Map.Pad[3]`); read it, don't re-parse it.
Use `v STORE.renium --json` to distinguish stored overrides from materialized defaults.
For a full saved-place diff, use `cmp BEFORE --full` from the target project. Add `AFTER` (or `--against AFTER`) for two files; no project is required, and Studio session services such as `ConfigureServerService` are skipped. Both place formats work in either position. The direction is BEFORE → project/AFTER. Without `--full`, `cmp` is script-only. Counts cover the whole place; request `--all` or `--values` only when needed. Float noise in CFrames, empty blobs against none and same-named siblings that swapped places are not differences; a member without a known default that every instance of a class carries on one side and none on the other is counted under `oneSidedProperties` instead.

The full report prints one line per difference (`changed Script ServerScriptService.Main: Source, @Tag`, `added Part Workspace.New`), including references, packages and serialized Terrain data; `--output-mode json` keeps the records. `--values` includes before/after values and source; these may contain credentials, so redact them before sharing. `--limit` caps entries, not counts; `--all` returns every difference.

Comparison ignores file-local IDs/history, sibling and tag order, line endings, explicitly stored class defaults, engine-managed values (cached physics and mesh data, weld state, `RBX_` migration markers, and `Lighting.Technology` once a place is on Unified lighting) and, against a project, saved fields the project never captures and instances with `Archivable` off; `engineManagedProperties`, `notCapturedProjectProperties` and `unsavedProjectInstances` count what was skipped. References compare by matched targets; duplicate subtrees use Renium's identity matcher. Without persistent identity, a rename, move or class replacement can appear as removal/addition. Only project services are compared against a project; two files compare their union of services. `notSerializedProjectProperties` counts project properties Roblox does not save as standalone fields (such as CollisionFidelity); these cannot be checked directly, but their saved data is still compared. This compares decoded saved content, not published assets, DataStores or runtime state.

For current Studio versus an old file, use the synchronized project. If a known sync failure/pending edit makes it stale, resolve that and use one `lst --wait`; don't open the old file, import it, start Play or dump both trees just to assemble a diff.
