<!-- renium-version: 0.6.3 -->
# Renium for agents

Call the `rbx` tool: the arguments as a list, or `steps` (several lists) to run commands in one call; put every read or check you already know you need into one call. Load the tool first when it is deferred; `rbx` in a shell only without it. Act directly: commands report a missing Studio or a sync problem themselves, so skip `status`, `cs` and `lst`.

## Scripts: find, read the part, edit, check

- Find: the prompt and `RENIUM/map.md` (in context) give the names. `rbx sg TEXT` (`-E` regex, `--in NAME` or `src/Folder`, `--not TEXT`, `-c` counts) prints the matching lines, those in a function under its `FIRST-LAST name` line; `rbx ss WORD` lists the scripts containing a word. Both show each script's line count.
- Read only the part: `rbx sr DataHandler --fn GetProfile --fn Save` or `rbx sr DataHandler:690-760 src/B.luau:1-60,200- ServerScriptService.Main` (a script goes by name, path or instance path). Take the function or range `sg` printed, not a guessed wide one. `rbx sr FILE` prints a script of 120 lines or more as its outline (`--all`: whole); `sg TEXT --fn` prints the functions with a hit at once. Not `cat`, `grep` or a file read: these print less.
- Edit: `rbx se FILE OLD NEW [OLD NEW ...] [-f FILE OLD NEW ...]` replaces each OLD (exact text, once in its file), checks syntax and pushes, all or nothing; the text `sr` printed is enough, and a miss prints the closest place. New script: its source as `stdin` to `rbx se src/ServerScriptService/Name.server.luau --new`. A file edited another way: `rbx ps FILE`. `pushed, verified` means Studio holds the new source; with Live Sync on it delivers the file and nothing is pushed.
- Check in Play, one call: `rbx play -s --at Workspace.Lobby.Pad --until 'LUAU' --result 'LUAU' --client-result 'LUAU' --console 40 --stop-after` starts Play (`--at`: the player stands there), waits for the server expression (`workspace.DistributedGameTime > 10` for a timed run), `--client-until 'LUAU'` for one on client 1 (a panel is up) or `--until-console 'pong 42'` for a console line, then returns `result` and `clientResult`, prints both consoles and stops. After Play starts, `-t N` bounds each wait on its own: a wait that cannot come true costs all of it, so give it what the game itself needs (`-t 20` for a panel or a value, more for a round). When more questions may follow, leave out `--stop-after`, ask with `rbx l --server 'LUAU'` and `rbx lc 'LUAU' 1` (what they connect or schedule lives 120 s, so a later call can read a listener) and end with `rbx play -x`. After a code change `rbx play -r` restarts with the same options; `play -s` into a running session starts nothing (`alreadyRunning`). Consoles: `rbx co --server --level error`, `rbx co -p 1`.
- UI in Play, one call: `rbx inp -p 1 click "Main.Shop.Open" wait 300 text "abc" key Return click "Main.Shop.Redeem" --result 'LUAU' --shot FILE.png` runs the actions on client 1 in order (GUI paths are relative to PlayerGui), then returns the client expression and a screenshot; `rbx ui -p 1` lists the visible GUI paths.

## Instances

- Deeper than the map only when it matters: `rbx tr Workspace Lobby --depth 2`, `rbx in Workspace Lobby.Door -F props,attrs`; a service with no store yet is pulled from Studio by the first command that needs it. Studio state: one bounded `rbx l 'return ...'`. A `--where` edit lists every match it touched, so act directly and check that list.
- Add an instance: `rbx ba Workspace -I Lobby -n Pad -c Part -p 'Anchored=true' -p 'Size=[4,1,4]' -p 'Position=[0,0.5,0]' -p 'Material={"_type":"EnumItem","name":"Neon"}' --push`; `--push` sends it to Studio at once (files first, no review). Many edits, one push per store: `rbx ba --batch - --push` with `{"ops":[{"parent":"Workspace","name":"Props","class":"Folder","ref":"p"},{"op":"clone","path":"Workspace.Lobby.Pad","name":"Pad2","parent":"#p"},{"op":"set","path":"Workspace.Lobby.Door","props":{"Anchored":true}},{"op":"remove","path":"Workspace.Old"}]}` as `stdin` (piped in a shell); `rename` takes `name`, `move` takes `parent`.
- Change properties: `rbx bs Workspace Lobby.Pad -p 'Transparency=0.5' -p 'CanCollide=false' --push`; the service itself: `rbx bs Lighting -p 'Brightness=2.5' -p 'ClockTime=18.25' --push` (no target); attributes `-S attribute -p MaxSpeed=16`; tags `-p 'Tags=["Breakable"]'` (the whole list). Every match: `rbx bs Workspace Lobby --where 'is:Part Transparency>0.5' -p CanCollide=false --push` (`is:Class`, `tag:Name`, `Prop=v`, `<`/`>`, `or`). Copy: `rbx cp Workspace -i Lobby.Pad -I Props -n Pad2 --push`. Move or rename: `rbx mv Workspace -i Lobby.Pad -I Props --push`, `rbx rn Workspace -i Props.Pad Goal --push`; every match: `rbx rn Workspace -i Lobby --where 'Glass' --replace Glass Window_ --push`. Remove: `rbx br Workspace Lobby.OldPart Lobby.Sign --push` (several targets); every match: `rbx br Workspace Lobby --where 'Trim' --push`.
- Values are JSON, shaped by the property's type: `1`, `true`, `"text"`, `[x,y,z]` Vector3, `[x,y]` Vector2, `[r,g,b]` Color3 (0-255, or 0-1), `"Neon"` enum item, `"Bright red"` BrickColor, `[scale,offset]` UDim, `[xScale,xOffset,yScale,yOffset]` UDim2, `[min,max]` NumberRange, `{"_type":"CFrame","position":[x,y,z],"orientation":[rx,ry,rz]}` (degrees; or `Position=[x,y,z]` and `Orientation=[rx,ry,rz]` on anything with a CFrame, folded in), `"Workspace.Lobby.Pad"` for an instance reference (PrimaryPart, Attachment0, SoundGroup, an ObjectValue's Value); `rbx bg` shows any stored form.
- Only what no command covers (computed edits): `rbx l --edit 'LUAU'` applies in Studio as one undo step and returns the chunk's value, so return what verifies it; `rbx pl -s SERVICE` afterwards copies the result into the files.
- A push result is the Studio check: `verified: true` means the place holds every pushed value, `studio` shows them, `verified: false` names what Studio did not take. No `rbx l` or `rbx in` afterwards.

## Choose the smallest sufficient check

- Saved code/data: read files, focused assertions or offline queries; small edits need no Play.
- Runtime behavior: Play answers one specific question (input, replication, physics, errors); know the expected result first.
- Visual behavior: capture the real client with real input (`rs`, `inp`, `re`) and read `rf` frames; scripts cannot see it.

With Live Sync on, trust file edits: no push, poll or Play after each save; `lst --wait` only after a reported problem. A `ps` over 5 changes (default threshold) applies nothing and returns its change list unless its script files are named on the command line; check it, then pass `--yes`. Reuse Play sessions, batch checks, never stop a user's session; wait with `rbx wait EXPR -t N`, or `rbx wait 30s` for a fixed time. `rbx ck FILE` checks syntax without pushing; never `loadstring`.

## Guides

When the recipes do not cover the task, read the guide once; reread after a reported update. `RENIUM/data.md` stores and scripts; `RENIUM/data-properties.md` bulk, protected properties; `RENIUM/data-inspect.md` file queries; `RENIUM/configuration.md`; `RENIUM/sync.md` push, pull, Live Sync, conflicts; `RENIUM/playtest.md`; `RENIUM/runtime.md` live Luau, consoles; `RENIUM/network.md`; `RENIUM/input.md` UI, input, movement; `RENIUM/capture-device.md`; `RENIUM/performance.md`; `RENIUM/performance-profiler.md`; `RENIUM/projects.md` models, places; `RENIUM/links.md` Git, collaboration; `RENIUM/opencloud.md`; `RENIUM/opencloud-resources.md`; `RENIUM/opencloud-assets.md`; `RENIUM/advanced.md` Studio lifecycle, audio; `RENIUM/publishing.md`; `RENIUM/plugins.md`; `RENIUM/native-compatibility.md`.

## Targeting

Single-place projects use `src`; experiences `places/<alias>/src`, and at the experience root add `--place <alias|placeId>`. Two Studios on one project: `--place <placeId|window name>` first.

Stores are compressed (no grep): `f`/`bg`/`bb` read saved data, `q` a closed place, `l` Studio (`l` tables over 128 keys are cut). After a failure check state first.

Fix and verify defects in requested work. Report `autoDesyncedPackages`. Publishing needs authorization. Say synced, not saved: `persistence: teamCreate` means Team Create saved it, `unsaved` means the user saves. Open Cloud uploads follow `RENIUM/publishing-validation.md`.

## Tools and boundaries

- Pass arguments or pipe JSON/code; no payload files. Luau through a shell: write strings as `[[text]]`, not quotes.
- Open a place once with `rbx so FILE`; `rbx ro` reopens. Never take focus or global input.
- No project backups: `rbx rev` restores files; Studio undo covers pushes.
- `rbx upd` updates when asked or reported; reread guides.
- On a Renium failure follow `RENIUM/bug-reports.md`: write, scrub private data, upload, return the ID.
