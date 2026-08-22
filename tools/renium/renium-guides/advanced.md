# Less common operations

Read `RENIUM.md` first. These commands are direct: don't bind a context, start a daemon, or create a payload file.

```powershell
rbx status
rbx ro
rbx ro E:\Downloads\Place.rbxl
rbx sx --save
rbx sx --terminate
rbx pa <PLACE_ID> "Place Name" --game-id <GAME_ID> --alias main
rbx pn <PLACE_ID> lobby
rbx po <PLACE_ID> <OTHER_PLACE_ID>
rbx me src/ServerScriptService/Main.server.luau oldText newText
rbx inp -p 1 click "Shop.BuyButton" wait 100 key E
```

`status` reads the selected Studio state. `ro` reopens the exact connected local file or published place Renium remembered; an explicit file overrides that target. `sx` closes the selected Studio. A local file requires either `--save` or `--terminate`, so Renium never chooses what happens to unsaved local work.

Place order uses published place IDs, not aliases. `pa`, `pn`, and `po` update the project and invalidate any old internal binding automatically.

`me FILE OLD NEW [OLD NEW ...]` applies several exact source edits in one operation. Add `--all` to replace every match and `--class` when creating a missing script file requires an explicit Roblox script class.

`inp` executes ordered input pairs. Supported actions are `click`, `right`, `move`, `down`, `up`, `right-down`, `right-up`, `scroll-up`, `scroll-down`, `key`, `kd`, `ku`, `text`, and `wait`. Mouse targets are a UI path or `x,y`; waits are milliseconds. Put `-p` before the action list.

Renium selects and binds the project/runtime itself. Ambiguity returns candidates instead of guessing. Correct a permanent error rather than trying a different command; Renium retries one transient connection failure internally.
