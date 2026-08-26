# Less common operations

Run these directly; don't prepare a daemon, context, or payload file.

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

`status` reads Studio state. `ro` reopens the remembered local file or published place; a file argument overrides it. `sx` closes Studio. Local files require `--save` or `--terminate`.

Place order uses published IDs. `pa`, `pn`, and `po` invalidate old bindings automatically.

`me FILE OLD NEW [OLD NEW ...]` applies exact source edits. `--all` replaces every match; `--class` sets the class of a new script.

`inp` runs ordered action/value pairs: `click`, `right`, `move`, `down`, `up`, `right-down`, `right-up`, `scroll-up`, `scroll-down`, `key`, `kd`, `ku`, `text`, and `wait`. Mouse targets use a UI path or `x,y`; waits use milliseconds. Put `-p` first.

Renium selects the project and runtime. Ambiguity returns candidates. Fix permanent errors; Renium retries one transient connection failure.
