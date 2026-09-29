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

`pr --world` needs an on-screen target; use `go` first. `go PATH` walks to the nearest point of the part's or model's bounding box and stops within eight studs; `--pos` targets an exact point. Both return the remaining distance. `inp move X,Y` moves the game's mouse: hover events fire, `Mouse.Target` follows it, and a following click reaches `ClickDetector`s. A point under Roblox's own UI (chat, player list, top bar icons) is still delivered, with that element hidden for the moment; the result then carries `throughSystemUi`, meaning a player could not have clicked there. The Roblox menu button's corner is reserved by the engine and stays refused.

Input goes through the game's own input pipeline: the cursor, window focus and physical input stay untouched. Escape belongs to CoreGui; use another key or an on-screen control.

For several ordered actions, use `rbx inp -p 1 click "Shop.BuyButton" wait 100 key E`.
