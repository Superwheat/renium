# UI, input, and world interaction

Read `RENIUM.md` first. Read `RENIUM/playtest.md` too when the task needs Play.

```powershell
rbx ui -p 2
rbx press "Shop.BuyButton" -p 2
rbx type "hello" --path "Chat.Box" --enter -p 2
rbx click 450 323 -p 2
rbx key E -p 2
rbx key W --hold-ms 700 -p 2
rbx goto "Workspace.Shop.Door" -p 2
rbx goto --pos "745,40,510" -p 2
rbx wait "workspace:GetAttribute('Ready') ~= nil" -c -t 20
```

Run `ui` first and reuse its `p` path exactly; paths are relative to `PlayerGui`, though a leading `PlayerGui.` is also accepted. Duplicate names use `Name[n]`; ambiguity returns candidates. `press --world` needs an on-screen target, so use `goto` first. Injected clicks can't fire `ClickDetector`; use a `ProximityPrompt` or game input path.

`goto` finishes within eight studs of its target so nearby interaction is possible; its result includes the final distance.

Input targets one Play window without moving the system cursor or taking focus. The orange native shield stops physical input from interrupting an active sequence. Roblox reserves Escape for CoreGui, so use the game's on-screen control or an alternate key.

Structured input uses `actions` with an `action` field, for example `{"player":"1","actions":[{"action":"click","path":"Shop.BuyButton"},{"action":"wait","ms":100}]}` piped to `rbx a input CX -J -`.
