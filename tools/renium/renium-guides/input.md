# UI and input

Use runtime input only when the requested check needs interaction. For a saved UI property, query the project instead.

```powershell
rbx ui -p 1
rbx pr "Shop.BuyButton" -p 1
rbx ty "hello" --path "Chat.Box" --enter -p 1
rbx clk 450 323 -p 1
rbx ky E -p 1
rbx ky W --hold-ms 700 -p 1
rbx go "Workspace.Shop.Door" -p 1
rbx go --pos "745,40,510" -p 1
rbx wait "workspace:GetAttribute('Ready') ~= nil" -c -t 20
```

Reuse paths from `ui`'s `p` field. Paths are relative to `PlayerGui`; its prefix is optional. `Name[n]` selects duplicates.

`pr --world` needs an on-screen target; use `go` first. `go` stops within eight studs and returns the distance. Injected clicks do not fire `ClickDetector`; use a ProximityPrompt or game input.

Input targets one Play window without moving the cursor or taking focus. The orange shield blocks interfering physical input. Escape belongs to CoreGui; use another key or an on-screen control.

For several ordered actions, use `rbx inp -p 1 click "Shop.BuyButton" wait 100 key E`.
