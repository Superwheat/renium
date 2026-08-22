# UI, input, and world interaction

Read `RENIUM.md` first. Read `RENIUM/playtest.md` too when the task needs Play.

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

Run `ui` first and reuse its `p` path exactly; paths are relative to `PlayerGui`, though a leading `PlayerGui.` is also accepted. Duplicate names use `Name[n]`; ambiguity returns candidates. `pr --world` needs an on-screen target, so use `go` first. Injected clicks can't fire `ClickDetector`; use a `ProximityPrompt` or game input path.

`go` finishes within eight studs of its target so nearby interaction is possible; its result includes the final distance.

Input targets one Play window without moving the system cursor or taking focus. The orange native shield stops physical input from interrupting an active sequence. Roblox reserves Escape for CoreGui, so use the game's on-screen control or an alternate key.

Use `inp` when order matters: `rbx inp -p 1 click "Shop.BuyButton" wait 100 key E`. Each action is followed by its target or value; no payload file is needed.
