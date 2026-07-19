# Renium Roblox Plugin Release Audit

Audit date: 2026-07-19

Baseline: `main` at `1eec6b5e8bf1ccbe32b38cb9a656f38cbd13e276`

This document preserves the pre-fix audit and its original line references. The verified behavior and code-quality fixes were implemented afterward. The requested module split was not performed, security hardening remained excluded, and no tests were added.

## Scope

This is a source-only audit of the complete Roblox plugin and the Renium callers that define its real behavior. It covers every handwritten Lua/Luau module bundled by `Renium.project.json`, including runtime export, Studio change capture, editor apply, review UI, automation, connection handling, schema handling, profiling, and bundled `rbx_dom_lua` consumers.

The generated Studio API schema and rbx-dom database were treated as data and their consumers were checked. Generated `.rbxm` and `.rbxmx` files were not reviewed as separate source because they duplicate the modules. Game source outside the Renium tool was excluded.

Roblox Studio was not run during the source-only audit phase. A finding was listed as confirmed only when the plugin code and its released Rust or extension caller established a reachable failure. Engine-timing questions that could not be settled from source were kept in a separate validation queue and checked before implementation.

Security hardening is intentionally outside this audit.

No source files were changed during the audit pass.

## Priority

- P1: can silently lose, omit, misapply, or incorrectly approve data; can break a primary command or leave Studio in a state different from the reported result.
- P2: real reliability, identity, lifecycle, or scaling failure with a narrower trigger or a recovery path.
- P3: polish, maintainability, or long-session cost that should not block ordinary use by itself.
- Q: a Luau-skill code-quality group rather than one isolated runtime failure.

## Category summary

| Category | Confirmed groups | Highest priority |
| --- | ---: | --- |
| Change capture and tracker lifecycle | 4 | P1 |
| Identity, paths, and references | 6 | P1 |
| Schema and value fidelity | 7 | P1 |
| Mutation transactions and retry safety | 6 | P1 |
| Review correctness and lifecycle | 10 | P1 |
| Automation runtime and command behavior | 8 | P1 |
| Resource use and hot-path performance | 13 | P1 |
| Architecture and error handling | 12 | Q |
| Luau idioms and dead code | 6 | Q |

There are 54 behavior/reliability groups and 18 Luau/code-quality groups. A group can cover the same root problem in several modules.

## Luau skill coverage

The skill was reread after the full-source pass. User examples such as `pcall` wrappers and `BridgePluginRuntime.module.lua:2426` were treated as evidence, not as the audit boundary.

| Skill rule | Result | Evidence |
| --- | --- | --- |
| Maintainable, non-redundant, production-ready code; no shortcuts or copied hardcoding | Finding | QA-1 through QA-3, QA-12, and LU-1 cover oversized responsibility sets, duplicated models, erased protocol types, copied release metadata, and dead layers. No separate prototype or TODO implementation was found in the active plugin paths. |
| Think before coding; establish evidence and expose uncertainty | Clean audit process | Every behavior finding was checked against the released Rust or extension caller. Studio-dependent uncertainty remains in the validation queue rather than being presented as fact. |
| Inspect conventions and report suboptimal code without unrequested edits | Clean audit process | The project layout and caller contracts were inspected. Only this audit file changed; no Luau source or tests were added. |
| Simplicity first; avoid impossible error handling and speculative structure | Finding | QA-1, QA-4, QA-11, LU-1, and PF-12 cover redundant layers, protected calls around deterministic code, repeated same-route checks, dead helpers, and speculative batch construction. |
| Treat game-client input as untrusted, validate remotes on the server, and keep game state authoritative on the server | Not applicable | The plugin contains no `RemoteEvent`, `RemoteFunction`, `FireServer`, `InvokeServer`, `OnServerEvent`, or `OnServerInvoke` path. Its local JSON/WebSocket boundary was still checked for correctness in QA-5 through QA-10. Security hardening remains out of scope. |
| Structured script layout and coherent ownership | Finding | QA-1 through QA-3 identify the responsibility, codec, identity, and protocol ownership problems. |
| Centralized validation | Finding | QA-5 through QA-11 cover mismatched units, shallow mutation validation, silent widening or coercion, loose object/array checks, and repeated checks after the same boundary. |
| Expression-based initialization | Finding | LU-3 records the complete confirmed set and the side-effecting branches that were excluded. |
| String interpolation where text surrounds inserted values | Finding | LU-4 classifies all 150 handwritten concatenation expressions: 29 findings and 121 justified joins. |
| Branch structure based on independence, fixed case count, and growth | Finding | LU-2 records dispatch-table candidates; LU-6 records small mutually exclusive cases. Independent and priority-ordered conditions were excluded. |
| `pcall` only around operations expected to fail | Finding | QA-4 classifies all 233 protected calls: 112 confirmed cleanup sites, 95 justified boundaries, 6 intentional profiler probes, and 20 Studio-validation items. |
| Idiomatic conditions without losing real nil/false/true states | Finding | LU-5 records every confirmed redundant comparison and the tri-state or field-presence checks that must remain explicit. |
| Performance: complexity, allocations, repeated lookups, table construction, connections, and every-frame work | Finding | PF-1 through PF-13 cover each listed performance dimension. Tiny fixed-size helper work and unmeasured micro-optimizations were checked and excluded. |
| Prefer clear idiomatic code and reserve optimization claims for material work | Clean audit process | Findings are tied to unbounded state, event paths, full-tree work, large payloads, or permanent loops. Pure style micro-optimizations are not counted as runtime defects. |
| Full-codebase audit; do not let later examples replace existing rules | Clean audit process | Every bundled handwritten module and each current skill rule is represented here as a finding, a checked exclusion, or not applicable. |
| Final behavior, edge-case, trust-boundary, tool, and diff review | Clean with stated limits | Behavior and caller contracts were reread; available offline checks passed earlier. Studio was not run, unavailable lint/type tools are listed below, and the only working-tree change is this report. |

## 1. Change capture and tracker lifecycle

### CT-1 — P1 — Every `Camera` is excluded from change tracking

`BridgeStudioChanges.module.lua:487-503` returns early for any instance that `IsA("Camera")`. The later checks intended to exclude only `Workspace.CurrentCamera` and its descendants are therefore unreachable for every Camera.

User-created Cameras stored anywhere in a tracked service can be exported initially, but subsequent property, rename, reparent, add, and remove events are not observed by two-way sync.

### CT-2 — P1 — `onlyCodeMode` listener membership is not reconciled

`BridgeStudioChanges.module.lua:511-519`, `917-957`, `995-1003`, and `1106-1115` decide whether to connect an instance only when it is first encountered. `setOptions` changes the flag without adding or removing listeners.

An empty Folder added while only-code mode is active is skipped. Adding a ModuleScript later connects the script but not its Folder ancestors, even though those ancestors now satisfy the mode's own eligibility predicate. Folder renames and attributes are then missed. Turning only-code mode off also leaves every previously skipped instance unwatched.

### CT-3 — P2 — The watched service set only grows and out-of-scope entries leak into responses

`BridgeStudioChanges.module.lua:971-1031` can add a service but has no matching unwatch path. `buildStateResponse` at `1189-1213` filters `dirtyServices` by the current request but emits retained property changes and logs from every watched service.

After a service is removed from configuration, its edits continue accumulating. A request for service A can contain direct changes for removed service B. The extension rejects the mixed direct batch at `renium-vscode-extension/src/extension.ts:5228-5249`, falls back to a full A import, and only acknowledges A. B can contaminate later polls indefinitely.

### CT-4 — P1 — Change acknowledgments are not bound to the runtime that issued the sequence

`BridgeStudioChanges.module.lua:1156-1177` accepts only a numeric `ackSeq`; the state response at `1230-1249` contains no runtime identity. Rust clears runtime pins for each daemon request at `renium/src/main.rs:5435-5439`, and the extension polls and acknowledges in separate requests at `renium-vscode-extension/src/extension.ts:4258-4277`.

If runtime A reports sequence 100 and reloads before the acknowledgment, runtime B can receive `ackSeq=100` after starting its own counter at 1. B then clears changes that the extension never imported. `getBridgeInfo` already exposes `runtimeId`, so the missing binding is not an unavailable concept.

## 2. Identity, paths, and references

### ID-1 — P1 — Direct-change keys collapse different dotted names

`BridgeStudioChanges.module.lua:277-282` joins path segments with `"."`, and `directPropertyKey` at `338-340` uses that display string as identity.

`{"Workspace", "A.B", "C"}` and `{"Workspace", "A", "B.C"}` produce the same key when ordinals and property names match. One retained edit overwrites the other, and the later acknowledgment clears both sequence ranges.

### ID-2 — P2 — Reference fingerprints use non-unique `GetFullName()`

`BridgeStudioChanges.module.lua:743-750` fingerprints an Instance reference with `GetFullName()`. Duplicate-named siblings have the same full name.

Changing a reference between two such siblings can produce the same fingerprint and be skipped after the cache is primed. Structured paths elsewhere already carry ordinals, but this path drops them.

### ID-3 — P1 — Decimal and hexadecimal settings aliases collide

`BridgeEditorSync.module.lua:352-375` and `531-568` add both decimal and hexadecimal text aliases for each numeric instance index. The deletion keep-set at `657-675` treats any matching alias as the same identity.

Index 10 contributes decimal `"10"` while index 16 contributes hexadecimal `"10"`. If index 16 is desired and index 10 is obsolete, the desired alias for 16 can preserve 10 during an authoritative reconcile. Rust emits canonical hexadecimal settings IDs at `renium/src/main.rs:31184`; the decimal aliases create the ambiguity.

### ID-4 — P1 — Emitted `debugId` fallback is ignored during reference apply

`BridgeIdentity.module.lua:74-111` emits `pathSegments` plus `debugId` when a referenced instance is outside the exported service. Rust preserves that object in `renium/src/main.rs:9829-9845`.

`BridgeEditorSync.module.lua:867-890` checks only `settingsId` or `instanceId`; it never turns `raw.debugId` into the already-supported `debug:` identity. If the target moves and its old path no longer resolves, the decoder returns `nil` and clears properties such as `ObjectValue.Value`, `WeldConstraint.Part0`, or `Part1`.

### ID-5 — P2 — Automation paths cannot round-trip valid Instance names

`BridgeRuntimeApi.module.lua:20-28` emits dot-separated paths without escaping or duplicate ordinals. Readers at `302-308`, `404-425`, and `691-709` split on dots and interpret a trailing `[digits]` as an ordinal.

Names containing a dot, names that literally end in `[n]`, and duplicate-named siblings therefore produce returned paths that resolve to the wrong instance or cannot resolve. These paths are exposed as reusable addresses by `rbx ui`, execution results, and world commands.

### ID-6 — P2 — Source-only parent creation does not honor a missing duplicate ordinal

`BridgeEditorSync.module.lua:1645-1663` creates one Folder when a source change arrives without its structural upserts. If the requested parent ordinal is greater than one and that duplicate does not exist, the one newly created Folder is used anyway.

A recovery/source-only push can place a script under the wrong duplicate path. The fallback needs to create enough siblings or fail with the unresolved structured address.

## 3. Schema and value fidelity

### VF-1 — P1 — `PhysicalProperties.AcousticAbsorption` is dropped and ignored by equality

The runtime and Rust format preserve a sixth acoustic-absorption value, but `BridgeEditorSync.module.lua:949-960` rebuilds `PhysicalProperties` with only five arguments. `BridgeValueEquality.module.lua:114-121` also compares only density, friction, elasticity, and their weights.

Incoming absorption is reset to the constructor default. If only absorption differs, equality can classify the value as unchanged and skip the write entirely.

### VF-2 — P1 — Modern `Content` properties are silently omitted by compact export

Rust maps both `ContentId` and modern `Content` to compact type 17 at `renium/src/main.rs:25107-25118`. All compact encoders at `BridgePluginRuntime.module.lua:2588-2591`, `2826-2829`, and `3058-3062` accept only Lua strings.

A live value whose `typeof` is `Content` returns `nil` from the encoder and is omitted from the property mask. Current properties such as `MaterialVariant.EmissiveMaskContent`, `VideoPlayer.VideoContent`, and `SurfaceAppearance.EmissiveMaskContent` can revert to defaults in the exported filesystem state. The legacy serializer at `BridgePluginRuntime.module.lua:1010-1013` already handles `Content`, proving the omission is a codec split rather than a deliberate unsupported type.

### VF-3 — P1 — Normal startup replaces the newer bundled schema with an older external schema

Startup builds a merged schema from bundled rbx-dom data and `BridgeStudioApiSchema` at `BridgePluginRuntime.module.lua:530-541`. `configurePropertyCandidates` at `1296-1333` then clears those maps and replaces them only with the caller payload.

The released Rust path loads `_external/rbx-dom/rbx_dom_lua/src/database.json` at `renium/src/main.rs:24617-24625` and `25324-25334`. That database's `InputBinding` lacks bundled properties including `PrimaryModifier`, `SecondaryModifier`, `Type`, `UIModifier`, `DisplayImage`, and `DisplayName`, which exist at `BridgeStudioApiSchema.module.lua:2730-2754`.

The ordinary export path therefore removes newer Studio properties from consideration even though the plugin shipped a schema specifically to add them.

### VF-4 — P1 — Source read failure is cached as a real empty script

`BridgePluginRuntime.module.lua:4620-4655` returns and caches `""` when the indexed script no longer exists or reading `Source` fails.

The caller cannot distinguish a legitimate empty script from a snapshot race, inaccessible source, or stale index. A failed read can therefore overwrite a non-empty filesystem source with an empty file and remain cached for subsequent chunks.

### VF-5 — P2 — Non-finite numbers are not encoded consistently

Compact top-level numbers have a wrapper, but vector, CFrame, sequence, range, physical-property, Runtime API, and Studio-change components are emitted as raw numbers in several serializers, including `BridgePluginRuntime.module.lua:2584-2640`, `BridgeRuntimeApi.module.lua:41-86`, and `BridgeStudioChanges.module.lua:346-380`.

`NaN` and infinities cannot be represented as ordinary JSON numbers. Depending on the path, an otherwise valid response can fail encoding or lose the value. One authoritative number codec needs to cover nested components as well as scalar properties.

### VF-6 — P2 — Luau execution results lose tuple positions and table key types

`BridgeRuntimeApi.module.lua:41-84` stringifies every table key. `1314-1317` appends serialized tuple values to an array.

`return nil, "second"` becomes a one-element result because assigning the serialized `nil` does not advance the array. Numeric key `1` and string key `"1"` collide, and arrays become objects with string keys. The CLI cannot reconstruct the returned Luau values it claims to expose.

### VF-7 — P2 — Malformed typed values are silently changed into valid zero-valued data

`BridgeEditorSync.module.lua:825-844`, `929-1068`, and `1108-1118` default missing or non-numeric components to zero for Float, NumberRange, Vector2/3, UDim/UDim2, Color3, BrickColor, CFrame components, Rect, sequences, and Ray. `BridgeUi.module.lua:1128-1141` applies the same zero substitution in review previews. Capture-probe colors also turn malformed entries into black at `BridgeRuntimeApi.module.lua:892-927`.

This is reachable through the released direct property command: Rust accepts any JSON value at `renium/src/main.rs:8205-8242`. Filesystem property values also remain generic JSON maps through `9568-9653`. For example, a requested `{ "_type": "Vector3", "x": 4 }` is reported as successfully applied as `Vector3.new(4, 0, 0)` instead of being rejected. Other decoders already reject malformed base64, enum, Font, and CFrame-length data, so silent numeric repair is not a consistent permissive contract.

## 4. Mutation transactions and retry safety

### TX-1 — P1 — `applyChanges` commits mutations made before a later failure

`BridgeEditorSync.module.lua:2660-2812` applies instance, source, and property changes sequentially. Each loop catches an error, sets `aborted`, and stops, but earlier mutations remain. The history recording is finished as a normal commit.

Rust rejects the push at `renium/src/main.rs:8096`, while Studio contains a partial batch. A class replacement followed by a source failure also skips reference retargeting, leaving references pointed at the detached old instance. The native-import path at `BridgeEditorSync.module.lua:2596-2610` already demonstrates rollback and history cancellation.

### TX-2 — P1 — Cancelling a chunked reconcile does not undo completed chunks

`BridgeEditorSync.module.lua:1919-2029` mutates the hierarchy during every reconcile chunk. Rust uses this protocol above 5,000 instances and calls `cancelEditorReconcile` after a later failure at `renium/src/main.rs:11748-11797`.

`cancelReconcile` at `BridgeEditorSync.module.lua:2652-2657` only deletes the session table entry. Created, moved, and replaced instances from prior chunks remain, and the final unknown-instance deletion phase never runs.

### TX-3 — P1 — Successful binary import is not replay-safe if its response is lost

`finishBinaryImport` removes its session before validating and applying it at `BridgeEditorSync.module.lua:2501-2508`. Rust allocates one request ID and retries transport failures across channels at `renium/src/main.rs:4152-4204`.

If the first finish call commits the import but its response is lost, the retry receives `"Native import session was not found"`. The CLI reports failure and skips later root-property batches even though the principal mutation succeeded. Chunk appends are duplicate-safe at `BridgeEditorSync.module.lua:2479-2481`; the terminal mutation is not.

### TX-4 — P1 — Request IDs are not used to prevent replay of mutating RPCs

Rust keeps one request ID while retrying a transport failure across sockets at `renium/src/main.rs:4152-4204`. `BridgeConnection.module.lua:209-261` validates the ID but neither remembers in-flight/completed IDs nor reuses the first result.

Any response lost after execution can run the method again. Concrete effects include:

- `executeLuau` can perform arbitrary Studio mutations twice;
- `requestEditorPushReview` can supersede the review created by its first attempt;
- `finishEditorPushReview` can display the review, remove the upload, and then report an incomplete upload on retry;
- `applyEditorChanges` can replay an already committed batch and produce a different result on its second pass.

TX-3 is the strongest committed-success-as-error example, but the root contract affects every non-idempotent method.

### TX-5 — P2 — Final export chunks are evicted before their responses are delivered

Final source, instance, defaults, and path chunk handlers remove their encoded cache at `BridgePluginRuntime.module.lua:4774-4854`. `BridgeConnection.module.lua:144-168` sends the response only after the handler returns.

If sending fails, Rust retries the same chunk request at `renium/src/main.rs:4486-4544`. The final chunk is regenerated after its cache was removed. If Studio changes between attempts, earlier chunks come from payload A and the retried final chunk comes from payload B, producing mixed or invalid assembled JSON.

### TX-6 — P2 — MeshPart protected-write authorization markers can survive indefinitely

`recentlyCreatedMeshPartKeys` is module-global at `BridgeEditorSync.module.lua:44`. Creation paths mark it at `1834`, `1988`, and `2064`; it is cleared only when a `MeshId` property change is processed at `2218-2247`.

Creating a MeshPart without a `MeshId` change leaves the path authorized. A later unrelated instance at the same structured path can inherit that stale authorization and bypass the normal protected MeshId review.

## 5. Review correctness and lifecycle

### RV-1 — P1 — Values over 512 bytes are classified as no-ops

Rust replaces large values with `_reviewTruncated` summaries at `renium/src/main.rs:9977-10007`. `BridgeUi.module.lua:1612-1638` leaves `haveNew` false for the marker and then marks every existing-instance change without a decoded new value as a no-op.

The summary rendering branch at `1644-1645` is therefore unreachable for the affected existing properties. A batch of large text or structured changes can fall below the threshold and apply without review.

### RV-2 — P1 — Authoritative reconcile deletions are omitted from review

Rust places `allowDeletes` inside each `instanceReconcile` entry at `renium/src/main.rs:10062-10081`. The UI only displays deletion language for a separate summary-row shape at `BridgeUi.module.lua:1470-1482`, which the current caller does not emit.

If desired instances are unchanged but an extra Studio instance will be deleted, the desired rows are filtered as no-ops, no deletion is shown, and the extra instance is removed by `BridgeEditorSync.module.lua:1853-1864`.

### RV-3 — P1 — Class replacements disappear because producer and UI use different field levels

Rust places `className` on the grouped row at `renium/src/main.rs:10037-10044`. The UI copies it into `group.className` at `BridgeUi.module.lua:2347-2353`, but the instance branches read `entry.className` at `1542-1567`.

The entry has only `kind` and `allowDeletes`. Replacing a Script with a LocalScript can produce no ClassName row and can be filtered as an unchanged instance.

### RV-4 — P1 — Stable-identity renames and reparents are invisible

The UI resolves an existing instance by stable identity at `BridgeUi.module.lua:1498-1507` but never compares its current name and parent with the desired structured path. `BridgeEditorSync.module.lua:515-527` later changes both.

A batch containing only moves or renames can bypass review as a no-op and then materially rearrange Studio.

### RV-5 — P1 — Chunked review compares grouped-row count with individual-change count

Rust groups many entries into one row at `renium/src/main.rs:10018-10055`, but sends the number of individual changes as `changeCount` at `10150-10178`. The plugin counts `#p.rows` and requires it to equal `changeCount` at `BridgePluginRuntime.module.lua:4913-4953`.

When a review exceeds 1 MiB and uses the chunk protocol, any grouped row makes `receivedRows < changeCount`. The terminal request reports an incomplete upload even though every chunk arrived.

### RV-6 — P1 — The single-slot review state has three race and retention failures

The state at `BridgeUi.module.lua:2215-2219`, replacement logic at `2368-2378`, and lookup at `2473-2491` are not a queue:

- An explicit Skip on review A is lost if review B replaces the already-decided state before A's next 300 ms poll. A then falls through to the default Apply response.
- If undecided A is recorded as a finished Skip but B is filtered below the threshold, A remains the active undecided state. It shadows the finished result after its countdown has been stopped, so Rust can wait until its 610-second deadline at `renium/src/main.rs:11573`.
- `finishedReviewDecisions` has no size or time limit. Superseded reviews whose callers disconnect remain for the plugin session.

### RV-7 — P2 — Duplicate-sibling ordinals are not displayed

The review tree uses `pathOrdinals` to resolve and key nodes at `BridgeUi.module.lua:1508-1517`, but flattened row labels at `1708-1845` contain names only.

`Door[1]` and `Door[2]` can render as indistinguishable `Workspace › Door` rows even though the plugin knows which sibling will change.

### RV-8 — P2 — Several structured values render incomplete or identical previews

`BridgeUi.module.lua:1185-1367` shows only a CFrame position and lacks complete component formatting for `ColorSequence`, `NumberSequence`, `Axes`, `Faces`, and `Ray`.

A rotation-only CFrame change displays identical old and new text. Other materially different values can render as the same datatype label even though decode and equality preserve the full value.

### RV-9 — P3 — Text truncation can split valid UTF-8

`BridgeUi.module.lua:1076-1090` validates with `utf8.len` and then measures and slices with byte-based `#` and `string.sub`.

A multibyte character crossing the byte cutoff becomes malformed in the RichText preview.

### RV-10 — P3 — Visible virtualized review rows keep stale theme colors

Rows embed current theme colors as literal RichText hex values at `BridgeUi.module.lua:1820-1885`. `applyStudioTheme` at `2542-2555` updates referenced containers but does not rerender the visible row pool.

Changing Studio theme while a review is open can leave unreadable old-theme text until scrolling or resizing causes another render.

## 6. Automation runtime and command behavior

### AR-1 — P1 — `wait-until` and `goto` spawn work that their runner immediately destroys

Rust injects an outer `task.spawn` for `wait-until` at `renium/src/main.rs:7175-7187` and `goto` at `7359-7377`. `BridgeRuntimeApi.module.lua:187-219` reports success when the outer chunk returns, then disables and destroys the generated Script.

The spawned helper returns control at its first yield, after which the runner is destroyed. `wait-until` works only when the first condition evaluation is already true; normal waits and movement can stop after their first side effect and later time out.

### AR-2 — P1 — Edit-mode timeout starts after pre-yield execution

`BridgeRuntimeApi.module.lua:1288-1304` calls `task.spawn` and only then creates the deadline. `task.spawn` resumes the chunk immediately.

CPU work performed before the first yield is excluded from the requested timeout. A non-yielding chunk can block the bridge until an unrelated engine or transport limit, despite the command reporting a bounded execution timeout.

### AR-3 — P1 — Edit-mode timeout cancels only the root task

The same branch retains and cancels only `executionThread`. Tasks created by the user's chunk are separate and the plugin runtime remains alive.

The response can claim `stopped=true` while descendant tasks continue mutating Studio after the timeout.

### AR-4 — P1 — Multiplayer count is clamped in Luau but treated as exact in Rust

`BridgeRuntimeApi.module.lua:1433-1455` clamps players to 1 through 8. `renium/src/main.rs:6721-6759` waits for the original unsigned value.

With 0, the plugin starts one client while Rust can report success as soon as the server appears. Above 8, Studio starts eight clients while Rust waits 90 seconds for the impossible original count and leaves the test running after failure.

### AR-5 — P2 — Console control markers can be skipped permanently

`BridgeRuntimeApi.module.lua:823-849` first selects the newest `limit` entries, then filters by `sinceSeq`, and returns `nextSeq=consoleSeq`. Rust advances that cursor and ignores `truncated` at `renium/src/main.rs:7240-7250`.

If a completion marker is followed by at least 100 log entries before polling, the marker is omitted and the cursor advances past it. `wait-until` or `goto` then times out despite completing.

### AR-6 — P2 — Captured command output has no byte or entry budget

Both runner and edit-mode execution append every print and warning to unbounded arrays at `BridgeRuntimeApi.module.lua:173-200` and `1261-1271`. The console history has a 1,000-entry count cap, but each message is also unbounded in bytes.

A command can allocate large output tables and only fail later when the response exceeds the 16 MiB transport limit. The error path itself must first allocate and encode the oversized response.

### AR-7 — P2 — Starting a test permanently writes a Workspace attribute

`BridgeRuntimeApi.module.lua:1387-1411` writes `Workspace.__ReniumPlace` before every test and never restores the previous value, including on startup failure. The only source reader is bridge labeling at `BridgePluginRuntime.module.lua:4236-4245`.

The tool can overwrite a user attribute or leave its private marker in the editable place where it can be saved and synced.

### AR-8 — P2 — The plugin accepts bridge-port settings that the daemon cannot support

`BridgeSettings.module.lua:78-99` floors decimal ports and imposes no channel-count limit. Persisted values at `113-166` do the same; one exact legacy `8781` through `8788` list is migrated, but other lists above four remain accepted. The connection settings UI saves that result at `BridgeConnection.module.lua:806-838`, and `774-798` creates one WebSocket channel for every accepted port.

Rust requires exact integer `u16` values and rejects more than four ports at `renium/src/main.rs:25077-25104`. A plugin setting therefore can be accepted and displayed even though no released daemon can use the same configuration. A long unique list also creates the same number of permanent reconnecting channels inside Studio.

## 7. Resource use and hot-path performance

### PF-1 — P1 — Retained Studio-change state can exceed the response limit and remain stuck

`BridgeStudioChanges.module.lua:284-335` retains one log per unique service/action/path/property key even after a full service snapshot is already required. `1200-1217` copies and sorts every retained entry. The transport rejects responses over 16 MiB at `BridgeTransport.module.lua:17-25`.

Once an ordinary poll cannot encode or send the state, it cannot acknowledge that state, so automatic polls repeat the same failure. Stopping live sync in the extension only stops its timer at `renium-vscode-extension/src/extension.ts:4163-4172`; it does not stop the plugin tracker or clear listeners and logs.

### PF-2 — P2 — Only-code mode still connects every existing descendant

`ensureService` unconditionally calls `connectExistingDescendants` at `BridgeStudioChanges.module.lua:959-1023`. Each descendant receives Changed and AttributeChanged listeners even when only-code mode was already enabled.

An event on an unrelated instance then runs recursive `FindFirstChildWhichIsA` eligibility checks. The mode reduces new listener creation but not the initial listener footprint or hot-event cost in a large existing place.

### PF-3 — P2 — Every direct property event scans preceding siblings at each ancestor

`BridgeStudioChanges.module.lua:384-412` calls `GetChildren()` and scans from the first child until the changed instance for every path segment.

Repeated CFrame or value changes on a late Workspace child allocate child arrays and do work proportional to unrelated siblings. This is the hot path intended to be cheaper than a full export.

### PF-4 — P3 — Tag discovery polls forever and never prunes old tag connections

`BridgeStudioChanges.module.lua:632-686` creates per-tag signal connections. `1049-1058` calls `GetAllTags()` every 0.5 seconds for the rest of the plugin session.

Connections for tags that disappear are never removed, so cost grows with every tag name observed during a long session. The permanent polling continues even when extension live sync is stopped.

### PF-5 — P2 — A supported binary import can hold at least twice its declared payload

The plugin accepts a 512 MiB binary import at `BridgeEditorSync.module.lua:2417`, retains all decoded chunk buffers, and allocates a second contiguous buffer at `2513-2520` before deserialization.

The advertised maximum can require roughly 1 GiB of raw buffers before adding the instance graph and deserializer allocations, creating severe memory pressure inside Studio.

### PF-6 — P3 — Reconnect watchdog runs at 20 Hz for the remaining plugin lifetime

After the first connection request, `BridgeConnection.module.lua:636-669` starts a loop that wakes every 0.05 seconds until plugin unload, including while all channels are healthy.

Most checks are timeout and reconnect state that do not need frame-like polling. Event-driven wakeups or a slower deadline-based timer would remove permanent background work.

### PF-7 — P2 — Direct property events read and encode large values twice

`BridgeStudioChanges.module.lua:697-818` builds a recursive string fingerprint for every relevant Changed event. The listener at `917-942` then calls `markDirectProperty`, which reads and encodes the same property again at `435-484`.

`Source` is direct-trackable, so editing a large script allocates a full `"string:" .. source` fingerprint and then reads and retains Source again for the outgoing change. The first read should produce the comparable fingerprint and transport value together instead of repeating the largest event-path work.

### PF-8 — P3 — One mouse query leaves a per-frame attribute writer running

`BridgeRuntimeApi.module.lua:762-820` installs a LocalScript the first time mouse location is requested. Its embedded code at `790-794` writes two attributes on every `RenderStepped`.

The probe is not removed or disconnected after the request; it remains active until the play session destroys PlayerScripts. One automation query therefore adds permanent per-frame property work for the rest of that session.

### PF-9 — P3 — The capped console buffer shifts 999 entries for every later message

`BridgeRuntimeApi.module.lua:109-120` appends a console entry and repeatedly calls `table.remove(consoleBuffer, 1)` above the 1,000-entry cap. After the buffer fills, every new log shifts the remaining array.

A noisy long playtest turns a bounded history into fixed O(1,000) table movement per message. A ring buffer would preserve the same sequence/cursor behavior without the repeated shifts.

### PF-10 — P3 — GUI automation repeats full-tree, ancestor, and sibling traversal

`BridgeRuntimeApi.module.lua:463-482` scans every PlayerGui descendant for an ID. Inventory at `628-682` also walks the full tree, then repeatedly climbs ancestors for visibility and clipping at `320-365` and rescans same-named siblings at each path level through `404-425`.

The public limit is 500 returned controls, but all preceding descendants and repeated path work still run. A single traversal with cached parent visibility, clipping, and sibling ordinals would remove the multiplicative work without changing the returned schema.

### PF-11 — P3 — Source indexing eagerly builds representations that each caller does not need

`BridgePluginRuntime.module.lua:1578-1637` builds stable-key paths, plain paths, ordinal paths, a key-to-instance map, an index array, and an index-to-instance map together. It also sorts `scriptIndices`, although scripts are appended while the instance index only increases at `4069-4099`.

The released normal export uses only index ranges at `4712-4760` and `renium/src/main.rs:29430-29468`; direct source lookup uses the key map at `4658-4664`. Building the representations lazily would avoid path and map work for the common range-only route.

### PF-12 — P3 — Shape compaction allocates a second complete batch before deciding to use it

The ordinary compact rows are built first. `BridgePluginRuntime.module.lua:2523-2548` then allocates one shaped row per item plus shape maps and rejects all of it when estimated savings are too small. The call site is `4534-4551`.

The released Rust caller always advertises shape support at `renium/src/main.rs:27084-27100`, so this cost runs for every qualifying production batch. Shape transport savings are useful; the polish issue is constructing the entire alternative before its viability is known.

### PF-13 — P3 — Binary transfer walks whole subtrees only to report counters

Before native serialization, `BridgeEditorSync.module.lua:2317-2347` calls `GetDescendants()` for every exported top-level child solely to calculate `instanceCount`; serialization then traverses the same graph. Import performs another set of full descendant counts after parenting at `2577-2588` only for response statistics.

These counts do not affect validation or mutation decisions. Large binary transfers can avoid the extra traversals or derive reporting from metadata already collected by the Rust serializer.

## 8. Architecture and error handling

### QA-1 — Q — Core modules combine too many unrelated responsibilities

The largest handwritten modules are:

- `BridgePluginRuntime.module.lua`: 5,339 lines
- `BridgeEditorSync.module.lua`: 2,827 lines
- `BridgeUi.module.lua`: 2,561 lines
- `BridgeRuntimeApi.module.lua`: 1,561 lines
- `BridgeStudioChanges.module.lua`: 1,319 lines

`BridgePluginRuntime` owns settings, role detection, schemas, property probing, five compact formats, caches, source transfer, RPC dispatch, profiling state, and tracker wiring. `BridgeEditorSync` owns path identity, datatype decode, source editing, instance replacement, reconcile sessions, binary sessions, history, protected writes, and reference retargeting.

This shared closure/module state makes lifecycle and transaction boundaries difficult to see. The stale schema, stale MeshPart marker, partial transaction, and retry bugs are direct examples of responsibilities interfering.

### QA-2 — Q — Datatype, path, and schema rules are duplicated instead of authoritative

Datatype behavior is independently encoded in legacy export, comparable values, compact v5, dispatch exporters, Studio direct changes, editor decode, equality, Runtime API serialization, and review formatting.

Path behavior is likewise split between structured paths, dotted paths, ordinal suffixes, debug identities, and several cache-key formats. The lost acoustic field, modern Content omission, incomplete review previews, and path collisions are already-proven drift caused by these copies.

One shared typed value model and one structured identity model should feed export, apply, equality, review, and automation formatting.

### QA-3 — Q — Protocol boundaries erase useful Luau types

Excluding generated schema/database files, 336 source lines contain `any` annotations or open `{ [string]: any }` maps. Dynamic JSON needs decoding, but the decoded request/response shapes remain untyped across internal module boundaries.

The review producer putting `className` on a group while the UI reads it from an entry, and `changeCount` meaning entries on one side and rows on the other, are mistakes a shared typed protocol shape would expose before runtime.

### QA-4 — Q — Protected calls are overused or scoped too broadly

All 233 `pcall`/`xpcall` occurrences outside generated schema/database files were classified:

- 101 wrap deterministic current-contract calls or internal helpers that already own their expected failures;
- 11 cover too much code and turn internal programming errors into fallback state;
- 95 are justified expected-failure boundaries;
- 6 are intentional profiler measurements of protected-call overhead;
- 20 depend on Studio API availability or lifecycle details and remain in the validation queue.

The 101 redundant sites are:

- `BridgeTheme.module.lua:34-36` and `40-42`
- `BridgeConnection.module.lua:934-939` and `945`
- `BridgePluginRuntime.module.lua:72-74`, `81-83`, `87-89`, `93-95`, `99-101`, `106-112`, `174-176`, `298`, `651-658`, and `724-726`
- `BridgeIdentity.module.lua:4-6`
- `BridgeInstanceSwap.module.lua:68`, `75`, and `102`
- `BridgeUi.module.lua:47-55`, `1503`, `2174-2182`, `2191-2193`, and `2495-2501`
- `BridgeRuntimeApi.module.lua:32-38`, `123-125`, `142-144`, `216-218`, `262-264`, `274-282`, `293-295`, `434-436`, `640-642`, `1217-1222`, `1286`, `1327-1335`, and `1549-1554`
- `BridgeEditorSync.module.lua:15-17`, `96-101`, `114-116`, `123`, `138`, `323`, `332`, `371`, `433`, `556`, `563`, `749-751`, `1076`, `1097`, `1136-1140`, `1163-1169`, `1193-1195`, `1356-1362`, `1375-1377`, `1459`, `1465-1472`, `1629`, and `2333`
- `BridgeProfiling.module.lua:286-288`, `634-636`, and `680-682`
- `BridgeStudioChanges.module.lua:164-166`, `268-273`, `500-517`, `581-602`, `638`, `650`, `659`, `677`, `744-746`, `790-792`, `839-841`, `859-883`, `899-901`, `906-943`, `960-962`, `1037-1043`, `1283-1285`, `1290-1292`, `1304-1306`, and `1311-1313`

The 11 overly broad sites are bundled-module loading at `BridgePluginRuntime.module.lua:310` and `325` and `BridgeEditorSync.module.lua:24`; whole exporters at `BridgePluginRuntime.module.lua:2063-2074`, `2082-2093`, `2151-2162`, `2400`, `3817`, and `3833`; and cache/background scopes at `3952` and `3990-4008`.

The justified set covers invalid JSON, UTF-8, base64, enum, Font, class, and Luau input; websocket operations; dynamic or protected properties and Source; `Instance.new` for caller-provided classes; async device, play, serialization, and mesh APIs; rollback writes; user code and request dispatch; and custom rbx-dom handlers. In particular, `BridgeUi.module.lua:1580`, `1596`, and `1614` and `BridgeEditorSync.module.lua:1173` and `1321` must stay protected because sequence constructors or custom rbx-dom handlers can throw despite the helper's normal result tuple.

The confirmed cleanup sites hide stack context, convert internal bugs into skipped tracking or cached fallback behavior, and make impossible states appear supported. Expected failures should remain protected at the smallest property or API boundary.

### QA-5 — Q — The same invariant is independently validated with different meanings

Several cross-language checks are repeated rather than assigning one authoritative normalized value and unit. The player limit is clamped by Luau while Rust waits for the original value. Review `changeCount` means individual entries in Rust and grouped rows in Luau. Plugin port parsing floors and accepts an unbounded list while Rust requires exact integers and at most four.

Repeated validation is appropriate at a transport boundary, but the normalized result or declared unit must be returned and reused. Re-deriving these contracts independently caused two P1 command/protocol failures and the reachable port-setting mismatch in AR-8.

### QA-6 — Q — Editor mutation batches are only shallowly validated before writes begin

`BridgeEditorSync.module.lua:2660-2680` validates list length, service, and the outer path before mutation. Nested descriptors are then coerced while applying: paths and ordinals are stringified or defaulted at `145-164`, source is size-checked only when already a string and later converted with `tostring` at `1670-1742`, instance classes and paths are defaulted at `1768-1899`, and property names are stringified at `2174-2185`.

A malformed later entry can therefore fail only after earlier entries changed Studio, amplifying TX-1. The current Rust producer uses typed structs at `renium/src/main.rs:2546-2623`, so this is saved as a protocol-boundary quality issue rather than another released-caller behavior finding. The plugin should validate and normalize a complete batch before opening history or mutating.

### QA-7 — Q — An invalid explicit service set silently becomes every service

`BridgeStudioChanges.module.lua:132-160` discards unknown service names and, when none survive, replaces the request with every allowed service. `getState` then starts tracking and applies reset or acknowledgment parameters to that widened set at `1252-1267`.

The released Rust caller rejects unknown and empty explicit lists at `renium/src/main.rs:29279-29304`, so ordinary Renium requests are safe. The plugin boundary still gives malformed input the opposite of least-surprising behavior: `"NotAService"` means all services instead of an error or an empty set.

### QA-8 — Q — Configuration RPCs report success after silently discarding malformed input

`BridgePluginRuntime.module.lua:1264-1341` clears schema and service caches before establishing that a property-candidate payload contains any valid entries. `1344-1391` coerces malformed export options to defaults. `BridgeStudioChanges.module.lua:1100-1104` ignores an invalid conflict mode, while the route at `BridgePluginRuntime.module.lua:5052-5059` saves the unchanged value and returns `ok = true`.

Current released callers send validated values, so this is not counted as a present command failure. The RPC contract should distinguish an intentional empty configuration from invalid input and return the normalized value or an error before destructive cache changes.

### QA-9 — Q — Binary import metadata accepts values that fail only later

`BridgeEditorSync.module.lua:2412-2464` checks that `totalBytes` is numeric but not that it is an integer. It accepts any table for `groups` and consumes it with `ipairs`, so an object, sparse array, or empty array can become a successful zero-group session. Group entries themselves are accessed before an explicit object check.

The typed Rust sender at `renium/src/main.rs:11602-11621` always emits integer sizes and a dense group vector. This remains boundary polish, but validation should reject malformed session metadata before allocating buffers or accepting chunks.

### QA-10 — Q — JSON arrays pass checks that promise request objects

`BridgeConnection.module.lua:209-245` rejects non-tables but cannot distinguish a decoded JSON array from an object. `BridgePluginRuntime.module.lua:4859-4861` then treats either as the params map, and many routes convert missing fields to empty strings, zeroes, defaults, or status requests.

The released Rust client sends JSON objects. The Luau boundary should still enforce the advertised object shape, especially for non-empty numeric arrays, instead of allowing method-specific accidental behavior.

### QA-11 — Q — The same request is revalidated down one unchanged route

After `BridgeConnection.module.lua:241-245` and `BridgePluginRuntime.module.lua:4859-4861` establish a params table, Runtime API methods repeat `params = params or {}` at `BridgeRuntimeApi.module.lua:463`, `628`, `685`, `823`, `852`, `936`, `1194`, and `1413`, with matching UI routes at `BridgeUi.module.lua:2305`, `2422`, `2453`, and `2473`.

Editor mutations preflight service and path at `BridgeEditorSync.module.lua:2664-2677` and immediately repeat the same checks in handlers at `1670`, `1760`, `1919`, `2036`, `2089`, and `2156`. Settings and property candidates are likewise normalized at several consecutive internal layers. These checks do not cross a new boundary and the relevant state has not changed; one normalized internal request type should carry the established invariant.

### QA-12 — Q — Release checks can miss protocol metadata drift

Protocol and codec names are copied into `BridgePluginRuntime.module.lua:209-215` and `renium/src/main.rs:119-129`. The release script checks only the product version string at `tools/build-release.ps1:148-159`.

A plugin/CLI protocol or codec edit can therefore pass packaging as long as all three product versions match. Build metadata should be generated from one source or the release check should compare every compatibility constant that must move together.

## 9. Luau idioms and dead code

### LU-1 — Q — Unreachable helpers remain in production source

Static reference checks found five local functions whose identifier appears only at its declaration:

- `BridgePluginRuntime.module.lua:2051` — `exportInstanceWithFallback`
- `BridgePluginRuntime.module.lua:2182` — `compactInstanceEntry`
- `BridgePluginRuntime.module.lua:2467` — `compactV5RowHasPropertyMask`
- `BridgePluginRuntime.module.lua:3843` — `exportCompactV5Instance`
- `BridgeStudioChanges.module.lua:575` — `serviceNameForDescendant`

`BridgeRuntimeApi.module.lua:1545` also exposes `bindRunStateHidden`, but no released caller invokes it. Dead compatibility and optimization layers make the active export paths harder to identify and review.

### LU-2 — Q — Large fixed case sets use long branch chains

The complete dispatch-table candidate set is:

- the 45-method RPC router at `BridgePluginRuntime.module.lua:4859-5180`, alongside the separate method registry at `5224-5270`;
- repeated runtime value routers at `BridgePluginRuntime.module.lua:925-1015`, `1496-1575`, `2574-2658`, `2660-2794`, `2812-2895`, and `3664-3702`;
- editor decode at `BridgeEditorSync.module.lua:929-1069`;
- schema type mapping at `BridgePropertySchema.module.lua:216-264`;
- equality routing at `BridgeValueEquality.module.lua:67-129`;
- Runtime API serialization at `BridgeRuntimeApi.module.lua:41-87`;
- direct-change encoding and stable formatting at `BridgeStudioChanges.module.lua:346-382` and `697-751`;
- review value formatting at `BridgeUi.module.lua:1185-1259` and `1261-1365`.

The method router is the clearest direct conversion because its registry already duplicates the same fixed key set. Datatype routes should first share one authoritative handler model rather than being mechanically converted into several unrelated dispatch tables.

### LU-3 — Q — Simple value selection is expressed through reassignment branches

The skill requires expressions when a condition only chooses a value. The complete confirmed set is:

- `BridgeContent.module.lua:4-9`
- `BridgeIdentity.module.lua:136-141` and `260-266`
- `BridgeEditorSync.module.lua:212-217`, `226-229`, `433-436`, `590-593`, `848-855`, `868-871`, `1017-1020`, `1540-1545`, and `1653-1656`
- `BridgeStudioChanges.module.lua:306-309` and `925-928`
- `BridgePluginRuntime.module.lua:226-233`, `677-683`, `873-880`, `1289-1292`, `1345-1348`, `1664-1667`, `2452-2455`, `3236-3239`, `3323-3328`, `3360-3363`, `3552-3557`, `3593-3596`, `3725-3733`, `3855-3862`, and `4204-4211`
- `BridgeRuntimeApi.module.lua:272-285`, `666-669`, `835-839`, `1239-1242`, `1433-1436`, and `1486-1489`
- `BridgeConnection.module.lua:108-112`, `286-289`, `452-461`, `807-810`, and `869-874`
- `BridgeUi.module.lua:217-225`, `693-696`, `1413-1418`, `1510-1513`, `1518-1525`, `1641-1648`, `1882-1902`, `1911-1931`, `2126-2133`, `2306-2311`, `2341-2344`, `2386-2404`, `2454-2456`, `2474-2476`, and `2510-2517`
- `BridgeStatus.module.lua:30-35` and `48-57`
- `BridgeProfiling.module.lua:83-86`

Conditional cache population, retry work, mutation of related values, and fallback calls with side effects were checked and excluded. For example, `BridgeEditorSync.module.lua:1707-1710` creates missing instances and updates statistics, so its branch does more than select a value.

### LU-4 — Q — Multi-value messages use concatenation where interpolation is clearer

All 150 concatenation expressions in handwritten bundled modules were classified. Twenty-nine contain surrounding prose, a value in the middle, or several inserted values and should use Luau interpolation:

- `BridgeContent.module.lua:16`
- `BridgeInstanceSwap.module.lua:64` and `103`
- `BridgePluginRuntime.module.lua:302`, `314`, `329`, and `4017`
- `BridgeRuntimeApi.module.lua:930`, `1049`, `1053`, `1055`, `1083`, `1108`, and `1125`
- `BridgeEditorSync.module.lua:1479`, `1688`, `1750`, `1829`, `1983`, `2059`, `2166`, `2213`, `2233`, `2242`, `2260`, `2274`, `2669`, and `2672`
- `BridgeUi.module.lua:1860`

The other 121 are intentionally not counted. They are simple prefixes or suffixes, machine-readable keys and identifiers, structured paths, WebSocket frame assembly, or incremental RichText construction where concatenation remains clearer.

### LU-5 — Q — Established truthiness is restated with redundant comparisons

`BridgePluginRuntime.module.lua:2426` is a direct example. The function deliberately maps both `false` and `nil` to the same compact-shape sentinel, so `if value == false or value == nil then` is exactly equivalent to `if not value then`.

The same category covers explicit `== true`, `== false`, `~= true`, `== nil`, and `~= nil` checks when the established value domain makes direct truthiness or a direct boolean coercion exactly equivalent. Confirmed sites are:

- `BridgeCandidateMatch.module.lua:13`
- `BridgeInstanceSwap.module.lua:7` and `17`
- `BridgeEditorSync.module.lua:73`, `445`, `453`, `459`, `465`, `674`, `696`, `759`, `1222`, `1235`, `1242`, `1249-1250`, `1253`, `1264`, `1412`, `1458`, `1683`, `1729`, `1737`, `1946`, `1959`, `2110`, `2309`, and `2798`
- `BridgeStudioChanges.module.lua:167`, `343`, `554`, `557`, `653`, `662`, `691`, and `972`
- `BridgePluginRuntime.module.lua:90`, `96`, `102`, `109`, `113`, `148`, `727`, `765`, `829`, `851`, `898`, `902`, `1130`, `1695`, `2012`, `2201`, `2426`, `3214`, `3249`, `3321`, `3433` for the denylist lookup only, `3479`, `3550`, `3759`, `3761`, `3769`, `3809`, `3812`, `3826`, `3830`, `4048`, `4075`, `4377`, `4382`, `4396-4397`, and `4811-4812`
- `BridgeRuntimeApi.module.lua:324`, `328`, `351`, `433`, `654`, `1069`, `1223`, `1306`, `1342`, `1348`, `1351`, `1354`, `1364`, `1424`, `1432`, and `1498`
- `BridgeUi.module.lua:50`, `163`, `169`, `320`, `524`, `532`, `836`, `846`, `1085`, `1161`, `1164`, `1370`, `1410`, `1428`, `1435`, `1487`, `1502`, `1516`, `1519`, `1530`, `1533`, `1543`, `1552`, `1556-1557`, `1579`, `1589`, `1595`, `1613`, `1622`, `1636`, `1662`, `1665`, `1714-1715`, `1731`, `1755`, `1808`, `1811`, `1874`, `1883`, `1912`, `2127`, `2131`, `2222`, `2226`, `2230`, `2237`, `2242`, `2248`, `2289`, `2368`, `2434`, `2462`, `2478-2479`, `2487`, `2515`, `2546`, and `2550`
- `BridgeStatus.module.lua:28` and `33`
- `BridgeTheme.module.lua:4`, `10`, `16`, `22`, and `37`
- `BridgeProfiling.module.lua:64`, `70`, `110`, `129`, `148`, `296`, `372`, `624`, `633`, `637`, `652`, `670`, `683`, `698`, and `730`

The exporter caches at runtime lines `3761`, `3769`, and `3826` contain a function after construction and use `false` or `nil` only to request construction, so both falsy states also have identical meaning there.

Exact input normalization, field-presence checks, strict boolean returns from optional-set lookups, and real sentinel or tri-state distinctions are excluded. Examples that must stay explicit include `BridgeEditorSync.module.lua:202`, `699`, `950`, `1385`, `1391-1392`, `1638-1642`, `1681`, `1797`, `1853`, `1897`, and `2720`; `BridgeStudioChanges.module.lua:508`; `BridgePluginRuntime.module.lua:856-859`, `1657`, `2123-2149`, `2393-2406`, and `5035`; `BridgeRuntimeApi.module.lua:477`, `622`, `649`, `840`, and `1415-1416`; `BridgeConnection.module.lua:38`, `237`, `835`, and `957`; `BridgeTransport.module.lua:34`; `BridgePropertySchema.module.lua:199`; `BridgeUi.module.lua:1124`, `1238`, `1241`, `1249`, `1318`, `1351`, `1354`, `1476`, `1540`, `1575-1576`, `1612`, and `1632`; `BridgeStatus.module.lua:50`; and `BridgeProfiling.module.lua:36`.

### LU-6 — Q — Small fixed case sets use separate `if` statements

These conditions are mutually exclusive, fixed, and too small to justify a dispatch table, so the skill calls for one `if`/`elseif` chain:

- `BridgeContent.module.lua:10-16`
- `BridgeEditorSync.module.lua:899-923`
- `BridgePluginRuntime.module.lua:517-527`, `2426-2443`, and `2800-2809`
- `BridgeRuntimeApi.module.lua:1058-1084` and `1390-1396`
- `BridgeSettings.module.lua:62-68`, `128-163`, and `188-213`
- `BridgeUi.module.lua:1117-1155`

No independent conditions were found incorrectly forced into an `elseif` chain. Ordering-sensitive fallback or priority logic was also excluded, including `BridgeConnection.module.lua:764-769`, `BridgePluginRuntime.module.lua:1398-1431`, generated hot-exporter chains at `3269-3318` and `3498-3547`, `BridgeEditorSync.module.lua:2128-2139`, and `BridgeRuntimeApi.module.lua:855-933` and `1415-1481`.

## Studio-behavior validation queue at audit time

These were saved but were not counted as confirmed from source alone:

1. Targeted attribute deletion sends JSON `null` inside an attributes object (`renium/src/main.rs:8194-8242`). The plugin only iterates surviving object keys at `BridgeEditorSync.module.lua:2254-2278`. Verify the exact Studio `HttpService:JSONDecode` null behavior before changing the protocol.
2. `BridgeConnection` handles `Error`, calls `Close`, and also handles `Closed` at `BridgeConnection.module.lua:560-629`. Verify whether the websocket implementation emits both callbacks for one failure; if so, failure count and backoff are updated twice.
3. Native import chooses the first incoming top-level Camera as `Workspace.CurrentCamera` at `BridgeEditorSync.module.lua:2570-2593`. Verify serializer root order and whether active-camera identity is preserved elsewhere before changing this behavior.
4. `DescendantRemoving` calls recursive `disconnectInstanceTree` for every event at `BridgeStudioChanges.module.lua:1005-1014`. Verify Roblox's event fanout/order for removed subtrees before classifying repeated traversal as a measured scaling defect.
5. Twenty protected-call groups depend on exact Studio version or lifecycle behavior and should not be removed from source evidence alone: `BridgePluginRuntime.module.lua:678-680`; `BridgeInstanceSwap.module.lua:71`; `BridgeUi.module.lua:218-220`, `859-861`, `1592-1594`, `2183-2185`, and `2415-2417`; `BridgeRuntimeApi.module.lua:879-883`; and `BridgeEditorSync.module.lua:1487-1489`, `1498-1500`, `1511-1513`, `1521-1523`, `1534-1536`, `1555-1557`, `1561-1563`, `1567-1569`, `1574-1576`, `1583-1585`, `1606-1610`, and `2323`. These cover AcousticAbsorption availability, reserved attributes, async widget behavior, RequestRaise, safe-area properties, ScriptDocument races, and ScriptEditorService behavior.

Implementation follow-up checked all five items. Attribute deletion now has an explicit protocol field, stale close callbacks are ignored, native import preserves the active camera, subtree removal no longer repeats recursive disconnect work, and protected calls remain only at verified failure boundaries. The AcousticAbsorption compatibility wrapper was retained after the supported Luau environment proved direct access can fail.

## Checked and not counted

- Concurrent normal bridge requests are bounded by per-socket mutexes and the editor operation gate; no unbounded request-concurrency issue was established.
- Unknown review IDs default to Apply by current contract. It is not counted alone; the confirmed review lifecycle race is what makes that fallback unsafe.
- Console history is capped at 1,000 entries, so it is not unbounded by count. Unbounded message bytes, lossy cursor advancement, and the full-array shift at the cap are separately counted in AR-5, AR-6, and PF-9.
- `setExportOptions` cache invalidation is incomplete in isolation, but the released startup order immediately clears service state during schema configuration.
- Snapshot structure and lazily fetched properties can observe different moments, but no atomic snapshot guarantee exists and the released caller deliberately disables pre-serialization.
- Clearing previous binary-export sessions is not reachable through the released gated caller.
- Reconcile `allowDeletes` omission on the finish chunk is not a bug in the current caller; only authoritative reconciles use that chunk protocol.
- Native import target paths match the service-root, Terrain, and StarterPlayer group shapes emitted by Rust.
- Weak settings IDs resolving after paths is intentional. The confirmed reference bug is specifically the emitted strong `debugId` being ignored.
- Source contents are intentionally summarized rather than displayed in review.
- Closing the review widget, countdown behavior, small-review bypass, play-mode bypass, and `displayPrompts="never"` follow explicit policy branches.
- Dynamic RichText values are escaped; no RichText injection issue was found.
- Candidate matching above its configured cap, locked-sibling replacement fallback, native marker handling, and explicit `Texture.Rotation` omission match existing intent/tests.
- PowerShell-rendered mojibake was not treated as a source bug. The byte-based UTF-8 slice is independently real.
- Unsupported rbx-dom variants that cannot reach the active schema were not reported.
- Capture-probe cleanup and runner event/listener cleanup are handled on ordinary paths. The mouse probe is separately counted in PF-8 because its per-frame writer remains active for the play session.
- Explicit truthiness comparisons not listed in LU-5 were retained when they normalize untrusted input, test field presence, preserve a strict boolean return, or distinguish real nil/false/true states.

## Offline verification performed

- `lune run tools/plugin_ws_bridge/tests/run.luau` — passed.
- `cargo test studio_bridge_modules_parse_as_luau --bin renium` from `tools/renium` — passed; one selected test, 159 filtered.
- Selene and StyLua wrappers were unavailable because their Aftman versions are not provisioned in this workspace.
- `luau-analyze` was not installed.

## Implementation verification

- The complete existing Rust suite passed: 159 passed and one timing test remained ignored.
- The complete existing extension suite passed: 19 passed.
- Plugin behavior tests and Luau parsing passed after the final edits.
- The rebuilt plugin loaded in Roblox Studio and connected all three bridge channels.
- The initial three post-change full syncs each exported 95,712 instances. Median total time changed from 7,610.5 ms to 7,469.1 ms, and median core export time changed from 1,430.3 ms to 1,330.4 ms.
- After the status fix, three final full syncs again exported 95,712 instances. Their median total time was 7,105.2 ms and median core export time was 1,270.5 ms.

## Post-audit status finding

### ST-1 — P2 — A completed Studio-to-editor sync leaves the status at `Waiting for sync`

`BridgeStatus.module.lua` uses `editorSyncStats.lastAtUnix` for the shared last-sync label. Before the follow-up fix, only editor-to-Studio apply and binary-import paths updated that field. The full export path released each service but had no whole-run completion signal, so a newly loaded plugin still displayed `Waiting for sync` after a successful full sync.

Per-service `release` was not a valid completion point because services can export concurrently and filesystem import, native-fidelity merge, generated project writing, and sourcemap work happen afterward. Renium now sends one success-only completion RPC after all of that work finishes. The plugin updates the shared timestamp without incrementing the editor mutation counters or replacing their duration.

The rebuilt Studio plugin accepted that completion RPC after each final full sync and refreshed the status UI without an error.

## Later fix order

When fixes begin, handle the P1 groups in this order:

1. Review omissions and lifecycle races.
2. Partial apply, reconcile rollback, and terminal request idempotency.
3. Change acknowledgment identity and change-capture blind spots.
4. Schema replacement, modern Content, PhysicalProperties, typed-value rejection, and source-read failure.
5. Settings/reference identity collisions.
6. Automation runner lifetime, timeout, multiplayer normalization, and port-count validation.
7. State/output bounds before performance-only refactors.

Each fix should recheck the caller contract listed here before editing. Code-quality refactors should follow behavior fixes unless a small extraction is needed to create one authoritative codec, identity, or transaction boundary.
