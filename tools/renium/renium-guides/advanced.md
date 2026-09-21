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

Renium dismisses Auto-Recovery with **Ignore** (preserving recovery files) and Lighting Technology Migration with **Continue**. On macOS, it also dismisses the known `BulkPluginAssetDetailsFetcher::sendRequest()` HTTP 500 startup alert with **OK**. Other dialogs still require their normal decision. On macOS, this and window-name targeting require Accessibility permission for the app launching Renium (for SSH, `sshd-keygen-wrapper`/Remote Login).

`sx` closes the target. Local files require `--save` or `--terminate`; termination discards unsaved work.

Launch or close Studio only when needed. Don't call `PluginManager:ExportPlace`: it opens a modal save panel on macOS. Use Renium's pull/export commands.

Place order uses published IDs. `pa`, `pn`, and `po` invalidate old bindings automatically.
Ambiguous targeting returns candidates. Transient reads retry automatically; after a lost mutation response, inspect the affected state before repeating it.

## Studio audio

`rbx audio mute` silences the selected Studio process; `unmute` explicitly unmutes
it. `rbx audio auto` mutes while another application or Studio window is focused
and restores only Renium's mute changes when you return. `rbx audio off` restores
those changes and stops automatic control. `rbx audio status` reads the mode,
focus and output-session counts. Use `--player 1` for a separate test client or
`--pid PID` for an exact local Studio process. Other Studio processes are untouched.

Add `--global` for a persistent user-wide mode covering existing and newly opened
Studio windows, without a project or connection: `rbx audio auto --global`.
Read it with `rbx audio status --global`; `off --global` or `unmute --global`
disables it and restores prior mute states, preserving manual mutes. Explicit
window commands override it until the next global change. The setting resumes
when Renium starts; it also keeps watching when all Studio windows are closed.
The same setting is **Renium: Studio Audio Mode** in editor User Settings, or
`rbx cfg set studioAudioMode auto --scope user` (`off`, `auto`, `mute`).
Settings and menu/CLI changes stay synchronized; project overrides are rejected.

Default: off. Without `--global`, control lasts until that Studio process closes,
including across daemon reconnects. No Sound objects or saved volume
levels are edited. Renium remembers its mute changes across audio-worker restarts
so returning focus restores audio. The editor offers **Renium: Studio Audio**. macOS requires
Studio opened with the matching Renium helper; global status lists any window
that needs reopening rather than reopening it automatically. Unsupported aggregate/virtual
output devices report an error instead of muting the system output.

## Publishing places

```powershell
rbx publish --dry-run
rbx --place lobby publish
rbx publish --open-cloud
rbx publish --open-cloud --file build.rbxl --universe 123 --place-id 456
```

Publishing requires user authorization. Default: publish the selected Studio Edit
state to its existing place with the Studio login, without pushing files first.
Settle pending Live Sync with `lst --wait`. Studio's Save Place API must be enabled
for the place; an active Team Create session blocks it. Don't change those settings
or credentials to work around a refusal without authorization.

`--open-cloud` builds the selected place project, or uploads `--file` unchanged.
Use `ROBLOX_API_KEY` / `--key-env ENV` with Universe Places Write. IDs come from the
experience or explicit flags. Cloud rejects instance types its API cannot update
reliably; use Studio for those. `--dry-run` validates the input/target without an
upload or permission check (cloud mode still builds). Temporary builds are cleaned.
Neither mode creates places, publishes packages, or restarts servers. An unconfirmed
response is not success: inspect Version History before retrying.

## Source edits and ordered input

```powershell
rbx me src/ServerScriptService/Main.server.luau oldText newText
rbx inp -p 1 click "Shop.BuyButton" wait 100 key E
```

`me FILE OLD NEW [OLD NEW ...]` applies exact replacements.
`--all` replaces every match; `--class` sets a new script's class.

`inp` accepts action/value pairs: `click`, `right`, `move`, `down`, `up`, `right-down`, `right-up`, `scroll-up`, `scroll-down`, `key`, `kd`, `ku`, `text`, `wait`.
Mouse targets are UI paths or `x,y`; waits are milliseconds. Put `-p` first.
