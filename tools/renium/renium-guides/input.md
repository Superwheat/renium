# UI and input

Use runtime input only when the requested check needs interaction. For a saved UI property, query the project instead.

```powershell
rbx ui -p 1
rbx pr "Shop.BuyButton" -p 1
rbx ty "hello" --path "Chat.Box" --enter -p 1
rbx inp -p 1 click 450,323
rbx inp -p 1 key E
rbx inp -p 1 hold W 700            # holds up to 60 s
rbx inp -p 1 drag "Settings.Volume.Knob" 315,430
rbx go "Workspace.Shop.Door" -p 1
rbx go --pos "745,40,510" -p 1
rbx wait "workspace:GetAttribute('Ready') ~= nil" -c -t 20
```

Reuse paths from `ui`'s `p` field. Paths are relative to `PlayerGui`; its prefix is optional. A click or press on an element the player cannot see fails and says why: `Main.HUD has Visible=false` (the nearest hidden ancestor), a ScreenGui with `Enabled=false`, a zero `AbsoluteSize`, or a center outside the screen or a clipping ancestor; a covered element names what covers it. The error then lists what takes a click now, nearest to the target first (`Clickable now: Main.DailyRewards.Header.Exit, ...`, up to 12 paths), so the retry needs no `ui` call. `Name[n]` selects duplicates in the client's own child order, which for replicated world parts can differ from Edit's.

`pr --world` needs an on-screen target; use `go` first. `go PATH` walks to the nearest point of the part's or model's bounding box and stops within eight studs; `--pos` targets an exact point. Both return the remaining distance. `inp move X,Y` moves the game's mouse: hover events fire, `Mouse.Target` follows it, and a following click reaches `ClickDetector`s. A point under Roblox's own UI (chat, player list, top bar icons) is still delivered, with that element hidden for the moment; the result then carries `throughSystemUi`, meaning a player could not have clicked there. The Roblox menu button's corner stays refused.

Input goes through the game's own input pipeline: the cursor, window focus and physical input stay untouched. Escape belongs to CoreGui; use another key or an on-screen control.

A Roblox prompt (favorite, purchase, permission) blocks virtual input: clicks land on the game under it and keys fail with "CoreGUI has keyboard focus". `rbx inp -p 1 dismiss` answers the prompt at the screen center (`dismiss X,Y` for another point) with its own Cancel/No/Close/OK button (never one that may buy), hides it when that press fails, and releases a focused Roblox text box. The result's `coreGui` lists `pressed`, `hidden`, `focusReleased` and `pressError`; `holdsKeyboard: false` means keys reach the game again, `menuOpen: true` means an open Roblox menu still holds them. A hidden prompt stays unanswered, so the game never gets its result. `rbx inp -p 1 press Yes` presses one Roblox button by label or `X,Y`. `press Buy` confirms a Studio test purchase: of a dialog that sells, the button that buys (or an OK beside Cancel) is pressed only in Studio where the dialog shows "This is a test purchase" (English Studio), and its other buttons only when they decline, close or acknowledge. A press that buys waits up to 15 s for the purchase to answer; `coreGui.buttonsNow` lists the buttons the prompt shows after any press (a purchase's success sheet closes with `press OK`). Outside Studio `press` only declines, closes or takes a lone OK. A mouse press where a prompt button stands fails and names the `press` that takes it, if one does.

For several ordered actions, use `rbx inp -p 1 click "Shop.BuyButton" wait 100 key E`. `--result LUAU` evaluates an expression on the same client after the actions and returns it as `result`; `--shot PATH` screenshots that client afterwards (`shot`). A `click` on a GUI path is verified wherever it stands in the list: when its turn comes the client scrolls the element into view, refuses it when hidden or covered, clicks it and checks that a button fired `Activated` once or a text box took focus (`verifiedClicks` counts both; other elements get the checks only). In a list that holds other actions too, a button that fires no event on the press (it opens a prompt or replaces itself) is counted in `unconfirmedClicks` and the list goes on; a list of clicks and waits alone fails on it. The first click that fails stops the list and nothing after it runs, `--result` and `--shot` included: `Action 3 of 5 (click Main.Shop.Redeem) failed; actions 1-2 ran, actions 4-5 did not: ...`. `textObserved: false` means text typed after a box click did not arrive in it. `down`, `up`, `su`, `sd`, `move` and `drag` on a GUI path read its point at their turn. `drag FROM TO [MS]` presses at FROM, moves every frame for MS (default 300, up to 10 s) and releases at TO. `down` holds the button until `up`, across calls too (`heldButtons`); each `move` is its own event. A failed action releases held keys, mouse buttons and gamepad input. Actions: `kd KEY`, `ku KEY`, `key KEY`, `hold KEY MS` (up to 60 s), `text STRING`, `click`/`right`/`down`/`up`/`su`/`sd`/`move`/`drag` with a GUI path, a `Workspace.` part path or `X,Y`, `wait MS` (`300ms`, `0.3s`; up to 60 s), `dismiss [X,Y]`, `press LABEL|X,Y`. A key held with `kd` stays held across later `inp` calls until `ku`; the result lists `heldKeys` and `keysObserved` (whether the client saw each key change).

A gamepad works on a Windows Play client (`-p N`). Its buttons go through the key actions `key`/`kd`/`ku`/`hold` under their KeyCode names: `ButtonA`, `ButtonB`, `ButtonX`, `ButtonY`, `ButtonL1`, `ButtonR1`, `ButtonL2`, `ButtonR2`, `ButtonL3`, `ButtonR3`, `ButtonStart`, `ButtonSelect`, `DPadUp`, `DPadDown`, `DPadLeft`, `DPadRight`. `stick L|R X,Y` sets a thumbstick (X and Y from -1 to 1, +Y is up) and `trigger L2|R2 VALUE` an analog trigger (0 to 1); both stay as set until set again, so center a stick with `stick L 0,0` and let a trigger go with `trigger R2 0`. Held gamepad buttons stay held across calls like keys. A virtual controller connects on the first gamepad action, in the first free slot (`Gamepad1` unless a real controller holds it), and disconnects when Play ends. The result names the slot in `gamepad` and lists `heldGamepadButtons`, `gamepadSticks` and `gamepadTriggers`; `gamepadObserved` is what the client saw.

```powershell
rbx inp -p 1 key DPadDown wait 200 key ButtonA
rbx inp -p 1 stick L 0,1 wait 500 stick L 0,0
```
