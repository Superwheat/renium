# Playtests, Luau, and consoles

```powershell
rbx play -s                         # ordinary Play; default for one-client checks
rbx play -s --players 1             # local server plus one separate client
rbx play -s --players 2             # local server plus two clients
rbx cs
rbx l "return game.PlaceId"         # Play server during a test
rbx lc "return game.Players.LocalPlayer.Name" 2
rbx co --server -n 20
rbx co --player 2 -n 20
rbx play -x
```

Use ordinary Play unless a separate server or several clients are needed. `--players 1` starts a local server and client. `mode: "play"` identifies ordinary Play.

Outside Play, `rbx l` uses the edit DataModel without `LocalPlayer` or `PlayerGui`. During Play, `l` targets the server and `lc ... <name|index>` targets one client. Luau errors and timeouts exit nonzero.

Return values from `l` and `lc` instead of printing them. Captured `print` and `warn` output is returned without entering Studio Output. Use `co` for game or Studio messages.

Don't leave `l` or `lc` waiting while issuing another command; operations run in order. Register an observer, return, act, then read its state.

`l` and `lc` runners are removed on return. Use a temporary source script for persistent test fixtures.

In PowerShell, wrap Luau containing double quotes in single quotes; `\"` isn't an escape.

Pipe large programs to `rbx l -` or `rbx lc - PLAYER`.
