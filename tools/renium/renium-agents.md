<!-- renium-version: 0.5.9 -->
# Renium for agents

Use `rbx` from the place's project folder. Act directly: commands report a missing Studio or a sync problem themselves, so skip `status`, `cs` and `lst` checks. Results: one JSON line on stdout, errors on stderr.

## Recipes

- Saved data: `rbx tr Workspace Lobby --depth 2`, `rbx in Workspace Lobby.Door -F props,attrs`. Studio state: one bounded `rbx l 'return ...'`.
- Add an instance: `rbx ba Workspace -I Lobby -n Pad -c Part -p 'Anchored=true' -p 'Size=[8,1,8]' -p 'Position=[24,0.5,-12]' -p 'Material={"_type":"EnumItem","name":"Neon"}' --push`; `--push` sends that instance to Studio at once (files first, no review). Many at once: `'{"ops":[{"parent":"Workspace.Grid","name":"Tile_1_1","class":"Part","props":{...}}]}' | rbx ba --batch - --push`.
- Change properties: `rbx bs Workspace Lobby.Pad -p 'Transparency=0.5' -p 'CanCollide=false' --push`; attributes with `-a Health=50`. Move or rename: `rbx mv Workspace -i Lobby.Kiosk -I Shops --push`, `rbx rn Workspace -i Shops.Kiosk.Counter Register --push`.
- Values are JSON: `1`, `true`, `"text"`, `[x,y,z]` (Vector3), `{"_type":"Color3","r":1,"g":0.5,"b":0}`, `{"_type":"EnumItem","name":"Neon"}`, `{"_type":"UDim2","xScale":0,"xOffset":200,"yScale":0,"yOffset":50}`, `{"_type":"CFrame","position":[x,y,z]}`.
- Studio-only change, or tags and many edits at once: `rbx l --edit 'LUAU'` applies in Studio as one undo step and returns the chunk's value, so return what verifies it; `rbx pl -s SERVICE` afterwards copies the result into the files.
- New script: write `src/ServerScriptService/Name.server.luau`, then `rbx ck FILE && rbx ba ServerScriptService -n Name -c Script --push`. Edited scripts: `rbx ck FILE && rbx ps FILE` (nothing with Live Sync on).
- Play check: `rbx play -s --until 'workspace.DistributedGameTime > 10' --console 40 --stop-after` starts, waits, returns the server console and stops; or without `--stop-after`: `rbx l --server 'LUAU'`, `rbx co --server --level error`, `rbx play -x`.
- `verified: true` in a push result is the check; else one read.

## Choose the smallest sufficient check

- Saved code/data: read files, focused assertions or offline queries; small edits need no Play.
- Runtime behavior: Play answers one specific question (input, replication, physics, errors); know the expected result first.
- Visual behavior: capture the real client with real input (`rs`, `inp`, `re`) and read `rf` frames; scripts cannot see it.

Check syntax offline with `rbx ck FILE`, not `loadstring`. With Live Sync on, trust file edits: no push, poll or Play after each save; `lst --wait` only after a reported problem. A `ps` over 5 changes (default threshold) applies nothing and returns its change list; check it, then pass `--yes`. Reuse Play sessions, batch checks, never stop a user's session; wait with `rbx wait EXPR -t N`.

## Guides

When the recipes do not cover the task, read the guide once; reread after a reported update. `RENIUM/data.md` stores and scripts; `RENIUM/data-properties.md` bulk, protected properties; `RENIUM/data-inspect.md` file queries; `RENIUM/configuration.md`; `RENIUM/sync.md` push, pull, Live Sync, conflicts; `RENIUM/playtest.md`; `RENIUM/runtime.md` live Luau, consoles; `RENIUM/network.md`; `RENIUM/input.md` UI, input, movement; `RENIUM/capture-device.md`; `RENIUM/performance.md`; `RENIUM/performance-profiler.md`; `RENIUM/projects.md` models, places; `RENIUM/links.md` Git, collaboration; `RENIUM/opencloud.md`; `RENIUM/opencloud-resources.md`; `RENIUM/opencloud-assets.md`; `RENIUM/advanced.md` Studio lifecycle, audio; `RENIUM/publishing.md`; `RENIUM/plugins.md`; `RENIUM/native-compatibility.md`.

## Targeting and edits

Single-place projects use `src`; experiences `places/<alias>/src`, and at the experience root add `--place <alias|placeId>`. Two Studios on one project: `--place <placeId|window name>` first.

Edit scripts as files; stores are compressed (no grep). `f`/`bg`/`bb` read saved data, `q` a closed place, `l` Studio. Reuse IDs. Run mutations one at a time and read each result; after a failure check state first. A usage error (unknown flag, missing selector): fix the call and rerun. `l` tables over 128 keys are cut.

Fix and verify defects in requested work. Report `autoDesyncedPackages`. Publishing needs authorization. Say synced, not saved: `persistence: teamCreate` means Team Create saved it, `unsaved` means the user saves. Open Cloud uploads follow `RENIUM/publishing-validation.md`.

## Tools and boundaries

- Pass arguments or pipe JSON/code; no payload files.
- Open a place once with `rbx so FILE`; `rbx ro` reopens. Never take focus or global input.
- No project backups: `rbx rev` restores files; Studio undo covers pushes.
- `rbx upd` updates when asked or reported; reread guides.
- On a Renium failure follow `RENIUM/bug-reports.md`: write, scrub private data, upload, return the ID.
