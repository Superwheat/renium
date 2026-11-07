<!-- renium-version: 0.5.9 -->
# Renium for agents

Use `rbx` from the place's project folder. Act directly: commands report a missing Studio or a sync problem themselves, so skip `status`, `cs` and `lst` checks. Results: one JSON line on stdout, errors on stderr; skip `2>&1`.

## Recipes

- Saved data: `rbx tr Workspace Lobby --depth 2`, `rbx in Workspace Lobby.Door -F props`. Studio state: one bounded `rbx l 'return ...'`.
- Add an instance: `rbx ba Workspace -I Lobby -n Pad -c Part -p 'Anchored=true' -p 'Size={"_type":"Vector3","x":8,"y":1,"z":8}' -p 'Material={"_type":"EnumItem","name":"Neon"}' --push`; `--push` sends that instance to Studio at once (files first, no review).
- Change properties: `rbx bs Workspace Lobby.Pad -p 'Transparency=0.5' -p 'CanCollide=false' --push`. Values are JSON, e.g. `{"_type":"Color3","r":1,"g":0.5,"b":0}`.
- Many instances, or a Studio-only change: `rbx l --edit 'LUAU'` applies in Studio as one undo step; Live Sync (when on) pulls it into the files.
- New script: `rbx ba ServerScriptService -n PadCounter -c Script` creates its `.server.luau` under `src/ServerScriptService`; write it, `rbx ck FILE`, then `rbx ps FILE` (not needed with Live Sync on).
- Play check: `rbx play -s --until 'workspace.DistributedGameTime > 10' --console 40` starts, waits, returns the server console; `rbx l --server 'LUAU'` acts in it; `rbx play -x` stops.
- Verify with one read, then report.

## Choose the smallest sufficient check

- Saved code/data: read files, focused assertions, project checks or offline queries; small edits need no Play.
- Runtime behavior: Play answers one specific question about input, replication, physics or errors; know the expected result first.
- Visual behavior: capture the real client with real input (`rs`, `inp`, `re`), inspect `rf` frames; scripts cannot tell what the player sees.

Check syntax offline with `rbx ck FILE`, never `loadstring`. With healthy Live Sync, trust file edits: no push, poll or Play after each save; one `lst --wait` after a reported problem. A `ps` over 5 changes (default threshold) applies nothing and returns its change list; check it, then pass `--yes`. Reuse Play sessions, batch checks, never stop a user's session; wait with `rbx wait EXPR -t N`, not a shell loop.

## Read the relevant guide

When the recipes do not cover the task, read its guide once; reread after a reported update.

| Task | Guide |
|---|---|
| Saved instances and scripts | `RENIUM/data.md` |
| Bulk and protected properties | `RENIUM/data-properties.md` |
| File queries and comparisons | `RENIUM/data-inspect.md` |
| Configuration, imports | `RENIUM/configuration.md` |
| Push, pull, Live Sync, conflicts | `RENIUM/sync.md` |
| Play sessions | `RENIUM/playtest.md` |
| Live Luau and consoles | `RENIUM/runtime.md` |
| Network simulation | `RENIUM/network.md` |
| UI, input and movement | `RENIUM/input.md` |
| Captures, device simulation | `RENIUM/capture-device.md` |
| Performance, heap snapshots | `RENIUM/performance.md` |
| MicroProfiler, resource limits | `RENIUM/performance-profiler.md` |
| Models, places and packages | `RENIUM/projects.md` |
| Links, Git, collaboration | `RENIUM/links.md` |
| Open Cloud setup and fetch | `RENIUM/opencloud.md` |
| Open Cloud resources | `RENIUM/opencloud-resources.md` |
| Assets, products, analytics | `RENIUM/opencloud-assets.md` |
| Studio lifecycle, audio | `RENIUM/advanced.md` |
| Publish and diagnostics | `RENIUM/publishing.md` |
| Installed plugins | `RENIUM/plugins.md` |
| Native access and probes | `RENIUM/native-compatibility.md` |

## Targeting and edits

Single-place projects use `src`; experiences `places/<alias>/src`. A place folder selects its target; at the experience root add `--place <alias|placeId>`. Two Studios on one project: `--place <placeId|window name>` before the command.

Edit scripts as files; stores are compressed (no grep). `f`/`bg`/`bb` read saved data; `q` searches a closed place; `l` reads Studio. Reuse IDs. Run mutations one at a time, read each result, and after a failure check state before retrying. A usage error (unknown flag, missing selector) means fix the call and rerun. `l` tables over 128 keys are cut.

Fix and verify defects found in requested work. Report `autoDesyncedPackages`. Publishing needs authorization. Say synced, not saved: `persistence: teamCreate` means Team Create saved it, `unsaved` means the user saves. Open Cloud uploads follow `RENIUM/publishing-validation.md`.

## Tools and boundaries

- Pass arguments or pipe JSON/code; no payload files.
- Open a place once with `rbx so FILE`; `rbx ro` reopens it. Never take focus or global input.
- No project backups: `rbx rev` restores files; Studio undo covers pushes.
- `rbx upd` updates when asked or reported; then reread the guides.
- On a Renium failure follow `RENIUM/bug-reports.md`: write, scrub private data, upload, return the ID.
