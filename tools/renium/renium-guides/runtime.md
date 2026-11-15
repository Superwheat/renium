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

`l` targets Edit when stopped and the server during Play; `--server` and `--edit` pin it, and an error names where it ran (`[Edit window]`, `[play server]`, `[client 1]`). `lc CODE PLAYER` targets a client by name or index. Edit has no `LocalPlayer` or `PlayerGui`. Edit changes stay in the Studio session until saved, unless Team Create syncs them (see `RENIUM/publishing.md`).

`l` and `lc` read state. They cannot tell you what a player sees; when visibility is the question, record the client and look at the frames (`RENIUM/capture-device.md`), driven by `rbx inp` rather than by calling the game's functions.

`l` and `lc` print each returned value in the shortest form that reads: a string as it is, `nil`, `ok` for none, numbers to 3 decimals (3 significant digits below 1; a value just off a whole number keeps the difference, `5.0001`), `Vector3(1.235, 2, 3.5)`, `CFrame(x, y, z, rot rx, ry, rz)` (Orientation degrees, when rotated), `RGB(163, 162, 165)` for a Color3, `Enum.Material.Wood`, an Instance as `Workspace.Lobby.Door (Part)`, other tables as JSON. A run that returns one value prints a list of records as a header and a row per record with the keys sorted (`at:Vector3 name open` / `1,2.5,3 Door true`), a list of strings as a line each, and such lists inside a table under `key:`; a JSON table returned as a string prints as the table. `--output-mode json` keeps exact `results` with their `_type` objects (a table with more than 128 keys keeps the first 128 and carries `_truncated: {kept, total}`; tables with other keys are `{"_type":"Table","entries":[{key,value}]}`).

What a run costs is its code plus its result, so:

- Return the values and tables themselves. Don't build text with `string.format`, `tostring` or `JSONEncode`: the code is longer, and text you build prints as it is, up to 17 digits per number.
- Return the answer (a count, the three worst, the one that is wrong), not the data to find it in. A result past 12,000 characters is cut and kept whole in a file under `.renium/cache/` that the cut line names; `-o FILE` writes a result to a file and prints one line.
- Code you will run again is sent once: keep it in a file and pass the values, `rbx l -f probe.luau -a "Car 2" -a 5`, read in the file as `local name, n = ...` (`-a` takes JSON, or any other text as a string). A function stored in `shared` (`shared.speed = function(name) ... end`, later `return shared.speed("Car 2")`) lasts for the Studio session in Edit and the Play session on the server and each client, but keeps the `require` cache and the `print` of the run that defined it: define it again after a module changes, and have it return what it finds.
- How the code is spelled does not matter: spaces, line breaks and short names change the count by a percent or two.

To wait for runtime state, use `rbx wait EXPR -t SECONDS` (returns the value; 60 s unless `-t` says otherwise, up to an hour), never a shell loop of `l` calls with sleeps; `rbx wait 30s` (also `500ms`, `2m`) waits that long and nothing else, for a sampler's window. `l` and `lc` stop a run after 10 s (`-t N` raises it, up to 120); the default grows to cover `task.wait(N)` sleeps written into the code, and to 30 s for code that waits inside a loop. `EXPR` is an expression or a chunk with its own `return` (`'local p = workspace:FindFirstChild("P"); return p ~= nil'`); a timeout prints `{"ok":false,"timedOut":true,"value":...}` and exits 1, so no shell `timeout` is needed. `--result LUAU` (also on `play -s --until`) runs once after the condition holds and returns `result` with tables intact, replacing a separate `l --server` read; it may run for what is left of `-t`. A wait on a play runtime holds only that runtime; a wait in the Edit window polls in short slices and lets other commands run between them, so `rbx l --edit` can set what it waits for. `co` filters (`--level`, `--grep` regex, `-F` for plain text) search the whole retained console before the `-n` limit and report `scanned` and `matched`, so `matched: 0` means nothing matched, not that the filter was ignored. A runner error from a module that failed to load lists the lines Studio logged during the run.
In Edit, each `l` run requires ModuleScripts fresh from their current source, so module tables don't persist between runs; don't clone a module to reload it (a clone has no Parent, so its `script.Parent` is nil). During Play, `l` and `lc` share the running game's `require` cache.

Return values instead of printing. Luau errors and timeouts print `error: MESSAGE` plus traceback and exit nonzero; captured `print`/`warn` lines print first as `[level] message`, without entering Studio Output. `co` reads game/Studio messages: a header line with `nextSeq` (continue with `--since-seq`), then one `[level] message` line per entry; `--output-mode json` keeps entry objects with `time`. An empty `co` result means there were none; don't re-read `LogService` with `l` or `lc`. A stack naming `cloud_<id>` or `user_<name>` scripts comes from an installed Studio plugin, not the game.

Keep live queries bounded. Don't run nested descendant scans over saved data. Requests to different play runtimes (the server, each client) run concurrently; two requests to the same runtime run one after another, so a long `lc` on client 1 never holds up client 2. During Play, what an `l` or `lc` chunk connects or schedules (`:Connect`, `task.delay`, `task.spawn`) stays alive for 120 s after the chunk returns: one call arms a listener that counts into `_G`, a later call reads the count (a `print` after the return is not captured). In Edit, and after an error or a timeout, a run's threads end with it. To sample for longer or keep output, detach a runner: `rbx lc --detach NAME CODE PLAYER` returns as soon as the code first yields and keeps its threads alive (`--lifetime` seconds, default 600); record into the `state` table, act, then read it with `rbx lc --collect NAME PLAYER` (`--stop` removes it). The result carries `state`, captured `output`, `done` and the final `results`. A Renium call takes about 30 ms inside the process; the rest of what you measure is your shell.

For larger programs, pipe code to `rbx l -` or `rbx lc - PLAYER`, or run a file with `rbx l -f FILE` or `rbx lc -f FILE PLAYER` (`-a VALUE` passes it arguments). In PowerShell, single-quote code and double an embedded apostrophe; backslash does not escape PowerShell quotes. Windows PowerShell 5.1 also drops double quotes inside native arguments: there, write Luau strings as `[[text]]` or pipe the code.
