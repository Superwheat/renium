<!-- renium-version: 0.5.9 -->
# Renium for agents

Run `rbx` in the project folder. Act directly: commands report a missing Studio or a sync problem themselves, so skip `status`, `cs` and `lst`. Results: one JSON line on stdout (`l`/`lc`: the returned values as text), errors on stderr.

When an `rbx` MCP tool is present, call it with the arguments as a list (`steps` for a chain); otherwise run `rbx` in the shell.

## Recipes

- Layout: `RENIUM/map.md` (every store's instances two levels deep, refreshed by each `rbx` command) is the orientation; act from the prompt and the map. Deeper detail only when it matters: `rbx tr Workspace Lobby --depth 2`, `rbx in Workspace Lobby.Door -F props,attrs`; a service with no store yet (the map lists them) is pulled from Studio by the first command that needs it. Studio state: one bounded `rbx l 'return ...'`. A `--where` edit lists every match it touched (`instances`, `removed`, `renamed`), so act directly and check that list; no listing pass first.
- Read code by the part you need, not by the file: `rbx ss WORD` lists the scripts that contain a word, `rbx sg TEXT` (`-E` regex, `--in src/Folder`, `-c` counts per script) prints the matching lines with their numbers under each script's path, and `rbx sr FILE --start-line A --end-line B` prints a range. By function: `rbx sr FILE --outline` lists the functions with their lines, `rbx sr FILE --fn NAME` prints one, `rbx sg TEXT --fn` prints each function with a hit. `cat -n` and `grep -rn` put a number or a path on every line; read a whole script only when it is short or you are rewriting it.
- Add an instance: `rbx ba Workspace -I Lobby -n Pad -c Part -p 'Anchored=true' -p 'Size=[4,1,4]' -p 'Position=[0,0.5,0]' -p 'Material={"_type":"EnumItem","name":"Neon"}' --push`; `--push` sends it to Studio at once (files first, no review). Many edits, one push per store: `'{"ops":[{"parent":"Workspace","name":"Props","class":"Folder","ref":"p"},{"op":"clone","path":"Workspace.Lobby.Pad","name":"Pad2","parent":"#p"},{"op":"set","path":"Workspace.Lobby.Door","props":{"Anchored":true}},{"op":"remove","path":"Workspace.Old"}]}' | rbx ba --batch - --push`; `rename` takes `name`, `move` takes `parent`.
- Change properties: `rbx bs Workspace Lobby.Pad -p 'Transparency=0.5' -p 'CanCollide=false' --push`; the service itself: `rbx bs Lighting -p 'Brightness=2.5' -p 'ClockTime=18.25' --push` (no target); attributes `-S attribute -p MaxSpeed=16`; tags `-p 'Tags=["Breakable"]'` (the whole list). Every match: `rbx bs Workspace Lobby --where 'is:Part Transparency>0.5' -p CanCollide=false --push` (`is:Class`, `tag:Name`, `Prop=v`, `<`/`>`, `or`). Copy: `rbx cp Workspace -i Lobby.Pad -I Props -n Pad2 --push`. Move or rename: `rbx mv Workspace -i Lobby.Pad -I Props --push`, `rbx rn Workspace -i Props.Pad Goal --push`; every match: `rbx rn Workspace -i Lobby --where 'Glass' --replace Glass Window_ --push`. Remove: `rbx br Workspace Lobby.OldPart Lobby.Sign --push` (several targets); every match: `rbx br Workspace Lobby --where 'Trim' --push`.
- Values are JSON, shaped by the property's type: `1`, `true`, `"text"`, `[x,y,z]` Vector3, `[x,y]` Vector2, `[r,g,b]` Color3 (0-255, or 0-1), `"Neon"` enum item, `"Bright red"` BrickColor, `[scale,offset]` UDim, `[xScale,xOffset,yScale,yOffset]` UDim2, `[min,max]` NumberRange, `{"_type":"CFrame","position":[x,y,z],"orientation":[rx,ry,rz]}` (degrees; or `Position=[x,y,z]` and `Orientation=[rx,ry,rz]` on anything with a CFrame, folded in), `"Workspace.Lobby.Pad"` for an instance reference (PrimaryPart, Attachment0, SoundGroup, an ObjectValue's Value); `rbx bg` shows any stored form.
- Only what no command covers (computed edits): `rbx l --edit 'LUAU'` applies in Studio as one undo step and returns the chunk's value, so return what verifies it; `rbx pl -s SERVICE` afterwards copies the result into the files.
- New script: write `src/ServerScriptService/Name.server.luau`, then `rbx ck FILE && rbx ba ServerScriptService -n Name -c Script --push`. Edited scripts: `rbx ck FILE && rbx ps FILE`; `verified: true` means Studio holds the new source (nothing with Live Sync on).
- Play check, one call: `rbx play -s --at Workspace.Lobby.Pad --until 'LUAU' --console 40 --stop-after` stands the player on the instance (`--at` optional), waits for the server expression (`workspace.DistributedGameTime > 10` for a timed run; `untilSeconds` reports the wait) or, with `--until-console 'pong 42'` instead, for that text in either console (`untilConsole` says which side and how long); `--result 'LUAU'` and `--client-result 'LUAU'` then read server and client state in the same call (`result`, `clientResult`), so no second session; prints both consoles as text after the result line and stops. Longer or client side: `rbx l --server 'LUAU'`, `rbx lc 'LUAU' 1` (client 1), `rbx co --server --level error`, `rbx co -p 1` (client console), `rbx play -x`.
- A push result is the Studio check: `verified: true` means the place holds every pushed value, `studio` shows them, `verified: false` names what Studio did not take. No `rbx l` or `rbx in` afterwards.

## Choose the smallest sufficient check

- Saved code/data: read files, focused assertions or offline queries; small edits need no Play.
- Runtime behavior: Play answers one specific question (input, replication, physics, errors); know the expected result first.
- Visual behavior: capture the real client with real input (`rs`, `inp`, `re`) and read `rf` frames; scripts cannot see it.

Check syntax with `rbx ck FILE`, not `loadstring`. With Live Sync on, trust file edits: no push, poll or Play after each save; `lst --wait` only after a reported problem. A `ps` over 5 changes (default threshold) applies nothing and returns its change list unless its script files are named on the command line; check it, then pass `--yes`. Reuse Play sessions, batch checks, never stop a user's session; wait with `rbx wait EXPR -t N`, or `rbx wait 30s` for a fixed time.

## Guides

When the recipes do not cover the task, read the guide once; reread after a reported update. `RENIUM/data.md` stores and scripts; `RENIUM/data-properties.md` bulk, protected properties; `RENIUM/data-inspect.md` file queries; `RENIUM/configuration.md`; `RENIUM/sync.md` push, pull, Live Sync, conflicts; `RENIUM/playtest.md`; `RENIUM/runtime.md` live Luau, consoles; `RENIUM/network.md`; `RENIUM/input.md` UI, input, movement; `RENIUM/capture-device.md`; `RENIUM/performance.md`; `RENIUM/performance-profiler.md`; `RENIUM/projects.md` models, places; `RENIUM/links.md` Git, collaboration; `RENIUM/opencloud.md`; `RENIUM/opencloud-resources.md`; `RENIUM/opencloud-assets.md`; `RENIUM/advanced.md` Studio lifecycle, audio; `RENIUM/publishing.md`; `RENIUM/plugins.md`; `RENIUM/native-compatibility.md`.

## Targeting and edits

Single-place projects use `src`; experiences `places/<alias>/src`, and at the experience root add `--place <alias|placeId>`. Two Studios on one project: `--place <placeId|window name>` first.

Edit scripts as files; stores are compressed (no grep). `f`/`bg`/`bb` read saved data, `q` a closed place, `l` Studio (`l` tables over 128 keys are cut). After a failure check state first.

Fix and verify defects in requested work. Report `autoDesyncedPackages`. Publishing needs authorization. Say synced, not saved: `persistence: teamCreate` means Team Create saved it, `unsaved` means the user saves. Open Cloud uploads follow `RENIUM/publishing-validation.md`.

## Tools and boundaries

- Pass arguments or pipe JSON/code; no payload files. Luau through a shell: write strings as `[[text]]`, not quotes.
- Open a place once with `rbx so FILE`; `rbx ro` reopens. Never take focus or global input.
- No project backups: `rbx rev` restores files; Studio undo covers pushes.
- `rbx upd` updates when asked or reported; reread guides.
- On a Renium failure follow `RENIUM/bug-reports.md`: write, scrub private data, upload, return the ID.
