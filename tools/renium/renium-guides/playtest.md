# Play, live Luau, and consoles

## Decide before starting

Use saved files, offline queries, and focused tests first. A rename, constant change, source edit, or sync check does not require Play when its result can be verified programmatically.

Use `rbx ck FILE...` for offline Luau syntax checks. Don't wrap source in `loadstring`, run it, or require a module just to see whether it parses. Don't enable `LoadStringEnabled` or change place settings for validation. Successful syntax checks do not prove types or behavior; use the project's focused checks for those.

Start Play for a concrete runtime question that those checks cannot answer: replication between clients, input handling, physics, initialization, or an observed runtime failure. Choose the assertion or visual evidence before starting. Batch related checks and reuse a suitable running session; don't restart it for each edit.

## Sessions

```powershell
rbx status
rbx play -s                         # ordinary Play
rbx play -s --players 2             # local server and two clients
rbx play -s --players 2 --until 'shared.RoundHandler.GameState == "InRound"'
rbx play -r                         # restart after a code change (stops, then starts the same way)
rbx play --add-players 2            # join churn: 2 more clients join the running test (8 max); returns their names
rbx play --leave -p 2               # the server kicks client 2's player (PlayerRemoving fires) and closes its window
rbx cs
rbx play -x
```

Across add/leave cycles the play server's `Stats.InstanceCount` grows by about 300 per cycle while `#game:GetDescendants()` and memory stay flat (measured over three cycles): those are engine-side objects outside the DataModel that Studio's test server keeps per client that ever connected, not a game leak. Judge leaks by DataModel counts and `rbx perf heap`, not by InstanceCount.

Play keeps the scripts it started with; after editing server or client code, `rbx play -r` restarts the session. `--until EXPR` returns once the server expression is true (`--until-timeout`, default 120 s), so no loop is needed before the first test step.

Use ordinary Play for one-client checks. `--players 1` explicitly launches a separate server and client; `mode: "play"` means ordinary Play. Stop only a session you started or were asked to stop. `play -s`, `status` and `cs` give each runtime's Studio `pid`. File edits during Play can wait for Edit mode; that alone is not a sync failure.

Play runs the game's real server code. With Studio API access enabled, its DataStore writes (currency, inventories, progress) change the signed-in account's live data, so don't spend or alter saved data without asking.

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

`l` targets Edit when stopped and the server during Play; `--server` and `--edit` pin it, and an error names where it ran (`[Edit window]`, `[play server]`, `[client 1]`). `lc CODE PLAYER` targets a client by name or index. Edit has no `LocalPlayer` or `PlayerGui`.

`l` and `lc` read state. They cannot tell you what a player sees; when visibility is the question, record the client and look at the frames (`RENIUM/capture-device.md`), driven by `rbx inp` rather than by calling the game's functions.

Results are JSON: Luau arrays become arrays and string-keyed tables become objects (a table with more than 128 keys keeps the first 128 and carries `_truncated: {kept, total}`); tables with other keys come back as `{"_type":"Table","entries":[{key,value}]}`; `nil` is `{"_type":"Nil"}`; an Instance is `{"_type":"Instance","path":...}`; Vector3, CFrame, Color3 and enums carry their own `_type`. Return counts, slices or summaries, not whole tables.

To wait for runtime state, use `rbx wait EXPR -t SECONDS` (returns the value; up to an hour), never a shell loop of `l` calls with sleeps. `co` filters (`--level`, `--grep` regex, `-F` for plain text) search the whole retained console before the `-n` limit and report `scanned` and `matched`, so `matched: 0` means nothing matched, not that the filter was ignored. A runner error from a module that failed to load carries `consoleErrors` with the lines Studio logged during the run.
In Edit, each `l` run requires ModuleScripts fresh from their current source, so module tables don't persist between runs; don't clone a module to reload it. During Play, `l` and `lc` share the running game's `require` cache.

Return values instead of printing. Luau errors and timeouts exit nonzero; captured `print`/`warn` text returns to the caller without entering Studio Output. `co` reads game/Studio messages. An empty `co` result means there were none; don't re-read `LogService` with `l` or `lc`. A stack naming `cloud_<id>` or `user_<name>` scripts comes from an installed Studio plugin, not the game.

Keep live queries bounded. Don't run nested descendant scans over saved data. Requests to different play runtimes (the server, each client) run concurrently; two requests to the same runtime run one after another, so a long `lc` on client 1 never holds up client 2. Runners are removed on return. To observe over time, detach one: `rbx lc --detach NAME CODE PLAYER` returns as soon as the code first yields and keeps its threads alive (`--lifetime` seconds, default 600); record into the `state` table, act, then read it with `rbx lc --collect NAME PLAYER` (`--stop` removes it). The result carries `state`, captured `output`, `done` and the final `results`. A Renium call takes about 30 ms inside the process; the rest of what you measure is your shell.

For larger programs, pipe code to `rbx l -` or `rbx lc - PLAYER`. In PowerShell, single-quote code and double an embedded apostrophe; backslash does not escape PowerShell quotes. Windows PowerShell 5.1 also drops double quotes inside native arguments: there, write Luau strings as `[[text]]` or pipe the code.

## Network simulation

Use `net` for latency, jitter and packet loss. It does not start or restart Play. Configure an existing client by the same name/index used by `lc`:

```powershell
rbx net presets                              # Offline template list
rbx net show --player 1
rbx net set --player 1 --preset normal
rbx net set --player 1 --preset mid           # Change that client while Play stays running
rbx net set --player 2 --preset poor          # A different connection for client 2
rbx net set --player 1 --preset high --out-loss 0.2
rbx net restore --player 1                   # Restore values from before the first override
rbx net reset --player 2                     # Zero all six simulation values
```

| Preset | Minimum delay each way | Jitter each way | Packet loss each way | Use |
|---|---:|---:|---:|---|
| `normal` | 15 ms | 2 ms | 0% | Typical low-latency test |
| `mid` | 50 ms | 10 ms | 0.05% | Moderate latency |
| `high` | 100 ms | 15 ms | 0.1% | High latency |
| `poor` | 100 ms | 30 ms | 0.5% | Highly variable, lossy connection |

`poor` keeps jitter at 30 ms: Studio applies jitter cumulatively to a 20 Hz replication stream, so 100 ms made a client fall a whole round behind.

These are test templates, not measured device profiles. Added round-trip delay is at least twice the listed delay. Studio supports 0–1000 ms delay/jitter per direction and at most 0.5% loss; a loss value of `0.5` means **0.5%**, not 50%. Presets do not simulate outages or bandwidth caps.

For custom asymmetric conditions, use `--in-delay`, `--out-delay`, `--in-jitter`, `--out-jitter`, `--in-loss` and `--out-loss`. Inbound means server→client; outbound means client→server. A preset fills all six values; explicit flags override it. Without a preset, omitted settings stay unchanged.

`set`, `reset` and `restore` return the selected runtime/PID, applied settings and changed fields; trust a successful result instead of rereading. `show` returns `settings`, `active` (any non-zero value) and the matching `preset` or `null`. A readback does not prove gameplay works under those conditions. Large latency jumps trigger congestion control—step changes gradually when measuring steady state.

Without `--player`, `net show/set/reset` targets the selected Studio's settings in Edit. During Play, use `--player` to avoid changing the server/defaults. These settings are process-local; Renium refuses a shared-process layout that cannot isolate the requested client. Client overrides are restored when its plugin unloads, without undoing later manual changes. Explicit `restore` is useful before ending a test. An abrupt process crash cannot run cleanup.

This uses Studio's plugin API, not injected game code or OS-level throttling, and does not touch `IncomingReplicationLag`, which stays additive if enabled. See [Roblox's network simulation reference](https://create.roblox.com/docs/studio/testing-modes#network-simulation).
