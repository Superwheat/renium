# Studio lifecycle and place management

```powershell
rbx status
rbx ro
rbx ro fixtures/Place.rbxl
rbx sx --save
rbx sx --terminate
rbx pa <PLACE_ID> "Place Name" --game-id <GAME_ID> --alias main
rbx pn <PLACE_ID> lobby
rbx po <PLACE_ID> <OTHER_PLACE_ID>
```

`status` reads connection/play state. `ro` opens the remembered local file or published place; an explicit file overrides it. An already-open matching place is reused.

`ro` confirms launch, not bridge readiness. Run the needed Studio command next; it waits for connection. An immediate `status` can still show no clients during startup.

`--place DTE` matches the Studio window name, not `game.Name` (often `Place1`). Use a place ID or configured alias if the window title is unavailable. Duplicate window names require an unambiguous target.

On macOS, window-name targeting and automatic Auto-Recovery dismissal require Accessibility permission for the app launching Renium (for SSH, `sshd-keygen-wrapper`/Remote Login). Renium presses **Ignore**, preserving recovery files.

`sx` closes the target. Local files require `--save` or `--terminate`; termination discards unsaved work.

Launch or close Studio only when needed. Don't call `PluginManager:ExportPlace`: it opens a modal save panel on macOS. Use Renium's pull/export commands.

Place order uses published IDs. `pa`, `pn`, and `po` invalidate old bindings automatically.
Ambiguous targeting returns candidates. Transient reads retry automatically; after a lost mutation response, inspect the affected state before repeating it.

## Source edits and ordered input

```powershell
rbx me src/ServerScriptService/Main.server.luau oldText newText
rbx inp -p 1 click "Shop.BuyButton" wait 100 key E
```

`me FILE OLD NEW [OLD NEW ...]` applies exact replacements.
`--all` replaces every match; `--class` sets a new script's class.

`inp` accepts action/value pairs: `click`, `right`, `move`, `down`, `up`, `right-down`, `right-up`, `scroll-up`, `scroll-down`, `key`, `kd`, `ku`, `text`, `wait`.
Mouse targets are UI paths or `x,y`; waits are milliseconds. Put `-p` first.
