# UI, input, and world interaction

Also read `RENIUM/playtest.md` for Play tasks.

```powershell
rbx ui -p 2
rbx pr "Shop.BuyButton" -p 2
rbx ty "hello" --path "Chat.Box" --enter -p 2
rbx clk 450 323 -p 2
rbx ky E -p 2
rbx ky W --hold-ms 700 -p 2
rbx go "Workspace.Shop.Door" -p 2
rbx go --pos "745,40,510" -p 2
rbx wait "workspace:GetAttribute('Ready') ~= nil" -c -t 20
```

Run `ui` first and reuse its `p` path. Paths are relative to `PlayerGui`; a leading `PlayerGui.` also works. Use `Name[n]` for duplicates. `pr --world` requires an on-screen target, so use `go` first. Injected clicks can't fire `ClickDetector`; use a `ProximityPrompt` or game input.

`go` stops within eight studs and returns the final distance.

Input targets one Play window without moving the cursor or taking focus. The orange shield blocks interfering physical input. Roblox reserves Escape for CoreGui; use an on-screen control or another key.

For ordered input: `rbx inp -p 1 click "Shop.BuyButton" wait 100 key E`. Each action is followed by its value.
