# Play, live Luau, and consoles

## Decide before starting

Use saved files, offline queries, and focused tests first. A rename, constant change, source edit, or sync check does not require Play when its result can be verified programmatically.

Use `rbx ck FILE...` for offline Luau syntax checks. Don't wrap source in `loadstring`, run it, or require a module just to see whether it parses. Don't enable `LoadStringEnabled` or change place settings for validation. Successful syntax checks do not prove types or behavior; use the project's focused checks for those.

Start Play for a concrete runtime question that those checks cannot answer: replication between clients, input handling, physics, initialization, or an observed runtime failure. Choose the assertion or visual evidence before starting. Batch related checks and reuse a suitable running session; don't restart it for each edit.

## Sessions

Solo Play and Server & Clients support TeleportService travel to readable saved places on Windows and Apple silicon macOS. Multiplayer clients keep their windows, share one server per destination, and can rejoin the source server. Stop ends the whole test and preserves Edit. See [Playtest teleport](teleport.md) for compatibility mode and limitations.

One-client checks (UI, HUD, menus, a single car) use ordinary Play: `rbx play -s`. `--players N` launches a separate local server plus N client windows, which costs memory and GPU the owner notices; use it only for a test that needs several players, and `rbx play -x` it as soon as that test ends (`play -r` restarts a session as it was started, clients included). `--players 1` requires explicit `--mode play` when a separate server and one client are needed.

```powershell
rbx status
rbx play -s                         # ordinary Play: one window
rbx play -s --players 2             # local server and two clients
rbx play -s --players 2 --until 'shared.RoundHandler.GameState == "InRound"'
rbx play -r                         # restart after a code change (stops, then starts the same way)
rbx play --add-players 2            # join churn: 2 more clients join the running test (8 max); returns their names
rbx play --leave -p 2               # the server kicks client 2's player (PlayerRemoving fires) and closes its window
rbx cs
rbx play -x
rbx play --kill-orphans             # close test windows whose Edit window was closed or restarted
```

Across add/leave cycles the play server's `Stats.InstanceCount` grows by about 300 per cycle while `#game:GetDescendants()` and memory stay flat (measured over three cycles): those are engine-side objects outside the DataModel that Studio's test server keeps per client that ever connected, not a game leak. Judge leaks by DataModel counts and `rbx perf heap`, not by InstanceCount.

Play keeps the scripts it started with; after editing server or client code, `rbx play -r` restarts the session with the mode and player count it was started with (`-p N` or `--mode` override them; a session started from Studio's own buttons restarts as ordinary Play and `restartNote` says so). `--until EXPR` returns once the server expression is true (`--until-timeout`, default 120 s), so no loop is needed before the first test step.

`play -x` returns once the test's DataModels and windows are gone, so `play -s` can follow at once. A start Studio drops because the previous test is still ending or the place is still opening is requested again; the result then carries `retriedAfterMs`. `status` and `cs` list `orphans`: test windows whose Edit window has exited, which can block the next multi-client start. `play -s` closes the ones of its own place first (`closedOrphans`); `play --kill-orphans` closes them all.

`mode: "play"` means ordinary Play; a `note` in the result means a server plus client windows are running. Stop only a session you started or were asked to stop. `play -s`, `status` and `cs` give each runtime's Studio `pid`. File edits during Play can wait for Edit mode; that alone is not a sync failure.

Play runs the game's real server code. With Studio API access enabled, its DataStore writes (currency, inventories, progress) change the signed-in account's live data, so don't spend or alter saved data without asking.

## Live Luau and consoles

See [Live Luau and consoles](runtime.md).

## Network simulation

See [Network simulation](network.md).
