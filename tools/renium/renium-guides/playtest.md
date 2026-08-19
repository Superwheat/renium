# Playtests, Luau, and consoles

Read `RENIUM.md` first.

```powershell
rbx play -s                         # ordinary Play; default for one-client checks
rbx play -s --players 1             # local server plus one separate client
rbx play -s --players 2             # local server plus two clients
rbx clients
rbx l "print(game.PlaceId)"         # Play server during a test
rbx lc "print(game.Players.LocalPlayer.Name)" 2
rbx co --server -n 20
rbx co --player 2 -n 20
rbx play -x
```

Use ordinary Play unless the test needs a separate server runtime or multiple clients. `--players 1` is still a local-server test, not ordinary one-player Play. Ordinary Play still reports its internal `play-server` and `play-client` bridges; `mode: "play"` confirms it isn't a local-server test.

Outside Play, `rbx l` runs in the edit plugin context. It has the edit DataModel but no normal Play-client `LocalPlayer` or `PlayerGui`. Start Play before requiring runtime client code. During Play, `rbx l` targets the server and `rbx lc ... <name|index>` targets one client. Luau compile errors, runtime errors, and timeouts return nonzero.

Don't keep an `l` or `lc` command waiting while issuing another command; daemon operations run in order. Register any observer, return, perform the action, then read the recorded state.

An `l` or `lc` runner is removed when the command returns. Its callbacks and threads can't persist across later commands; add a temporary source script when a test fixture must persist.

In PowerShell, wrap Luau containing double quotes in single quotes; `\"` doesn't escape quotes there.
