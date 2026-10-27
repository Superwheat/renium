# Live Luau and consoles

See [playtest](playtest.md) for the main workflow.

## Query the right runtime

```powershell
rbx l "return game.PlaceId"
rbx l --server "return shared.RoundHandler.GameState"   # play server only; fails if none
rbx l --edit "return workspace.Name"                     # Edit window even during Play
rbx lc "return game.Players.LocalPlayer.Name" 1
rbx wait 'shared.RoundHandler.GameState == "InRound"' -t 300   # server; -p N for a client
rbx co --server --level error -n 20
rbx co -p 1 --grep "round|tag" -n 20
```

`l` targets Edit when stopped and the server during Play; `--server` and `--edit` pin it, and an error names where it ran (`[Edit window]`, `[play server]`, `[client 1]`). `lc CODE PLAYER` targets a client by name or index. Edit has no `LocalPlayer` or `PlayerGui`. An Edit result carries `persistence`: `teamCreate` means Team Create syncs what the code changed to Roblox by itself, with no save step; `unsaved` means it stays in the Studio session until the user saves or publishes (see `RENIUM/publishing.md`).

`l` and `lc` read state. They cannot tell you what a player sees; when visibility is the question, record the client and look at the frames (`RENIUM/capture-device.md`), driven by `rbx inp` rather than by calling the game's functions.

Results are JSON: Luau arrays become arrays and string-keyed tables become objects (a table with more than 128 keys keeps the first 128 and carries `_truncated: {kept, total}`); tables with other keys come back as `{"_type":"Table","entries":[{key,value}]}`; `nil` is `{"_type":"Nil"}`; an Instance is `{"_type":"Instance","path":...}`; Vector3, CFrame, Color3 and enums carry their own `_type`. Return counts, slices or summaries, not whole tables.

To wait for runtime state, use `rbx wait EXPR -t SECONDS` (returns the value; up to an hour), never a shell loop of `l` calls with sleeps. `co` filters (`--level`, `--grep` regex, `-F` for plain text) search the whole retained console before the `-n` limit and report `scanned` and `matched`, so `matched: 0` means nothing matched, not that the filter was ignored. A runner error from a module that failed to load carries `consoleErrors` with the lines Studio logged during the run.
In Edit, each `l` run requires ModuleScripts fresh from their current source, so module tables don't persist between runs; don't clone a module to reload it. During Play, `l` and `lc` share the running game's `require` cache.

Return values instead of printing. Luau errors and timeouts exit nonzero; captured `print`/`warn` text returns to the caller without entering Studio Output. `co` reads game/Studio messages: a header line with `count` and `nextSeq` (continue with `--since-seq`), then one `[level] message` line per entry; `--output-mode json` keeps entry objects with `time`. An empty `co` result means there were none; don't re-read `LogService` with `l` or `lc`. A stack naming `cloud_<id>` or `user_<name>` scripts comes from an installed Studio plugin, not the game.

Keep live queries bounded. Don't run nested descendant scans over saved data. Requests to different play runtimes (the server, each client) run concurrently; two requests to the same runtime run one after another, so a long `lc` on client 1 never holds up client 2. Runners are removed on return. To observe over time, detach one: `rbx lc --detach NAME CODE PLAYER` returns as soon as the code first yields and keeps its threads alive (`--lifetime` seconds, default 600); record into the `state` table, act, then read it with `rbx lc --collect NAME PLAYER` (`--stop` removes it). The result carries `state`, captured `output`, `done` and the final `results`. A Renium call takes about 30 ms inside the process; the rest of what you measure is your shell.

For larger programs, pipe code to `rbx l -` or `rbx lc - PLAYER`. In PowerShell, single-quote code and double an embedded apostrophe; backslash does not escape PowerShell quotes. Windows PowerShell 5.1 also drops double quotes inside native arguments: there, write Luau strings as `[[text]]` or pipe the code.
