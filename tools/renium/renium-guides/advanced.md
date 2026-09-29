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

`status` reads connection/play state. `ro` opens the remembered local file or published place; an explicit file overrides it. An already-open matching place is reused. Other places open in Studio belong to other work; never read or change them. With several Studio places open, bind a single-place project once: `rbx cfg set place GAME_ID:PLACE_ID`.

`ro` confirms launch, not bridge readiness. Run the needed Studio command next; it waits for the connection. `status -w SECONDS` waits while Studio is still starting and otherwise reports at once.

`--place DTE` matches the Studio window name, not `game.Name` (often `Place1`). Use a place ID or configured alias if the window title is unavailable. Duplicate window names require an unambiguous target.

Renium dismisses Auto-Recovery with **Ignore** (preserving recovery files) and Lighting Technology Migration with **Continue**. On macOS, it also dismisses the known `BulkPluginAssetDetailsFetcher::sendRequest()` HTTP 500 startup alert with **OK**. Other dialogs still require their normal decision. On macOS, this and window-name targeting require Accessibility permission for the app launching Renium (for SSH, `sshd-keygen-wrapper`/Remote Login).

`sx` closes the target. Local files require `--save` or `--terminate`; termination discards unsaved work. A published place closes without saving; Studio edits not yet saved to Roblox then exist only in the project files.

Launch or close Studio only when needed. Don't call `PluginManager:ExportPlace`: it opens a modal save panel on macOS. Use Renium's pull/export commands.

Place order uses published IDs. `pa`, `pn`, and `po` invalidate old bindings automatically.
Ambiguous targeting returns candidates. Transient reads retry automatically; after a lost mutation response, inspect the affected state before repeating it.

## Studio audio

`rbx audio mute` silences the selected Studio process; `unmute` explicitly unmutes
it. `rbx audio auto` mutes while another application or Studio window is focused
and always unmutes that window when you return, including a mute inherited from
an older session. `rbx audio off` releases Renium's suppression and stops
automatic control. `rbx audio status` reads the mode,
focus and output-session counts. Use `--player 1` for a separate test client or
`--pid PID` for an exact local Studio process. Other Studio processes are untouched.

Add `--global` for a persistent user-wide mode covering existing and newly opened
Studio windows, without a project or connection: `rbx audio auto --global`.
Read it with `rbx audio status --global`; `off --global` or `unmute --global`
disables it and releases Renium's suppression, preserving other manual mutes. Explicit
window commands override it until the next global change. The setting resumes
when Renium starts; it also keeps watching when all Studio windows are closed.
The same setting is **Renium: Studio Audio Mode** in editor User Settings, or
`rbx cfg set studioAudioMode auto --scope user` (`off`, `auto`, `mute`).
Settings and menu/CLI changes stay synchronized; project overrides are rejected.

Default: off. Without `--global`, control lasts until that Studio process closes,
including across daemon reconnects. No Sound objects or saved volume
levels are edited. Windows silences audio buffers inside the selected process;
it does not use the persistent Windows mixer mute for suppression. Focused Auto
and explicit `unmute` also clear an earlier Windows mute on that Studio session.
Suppression expires if its controller disappears. The editor offers **Renium: Studio Audio**. macOS requires
Studio opened with the matching Renium helper; global status lists any window
that needs reopening rather than reopening it automatically. Unsupported aggregate/virtual
output devices report an error instead of muting the system output.

## Save and publish state

Synced work is in the project files and the open Studio session. `rbx status` reports `teamCreate` and `placeVersion` for the selected Edit place: with Team Create, Studio saves every edit to Roblox itself, scripts included, and other collaborators' Studios receive it; without it, only File > Save to Roblox or a publish does, and Renium cannot see whether that happened. Players get changes only from a publish. When a collaborator in Studio still sees old behaviour, the code is the suspect, not the sync: `rbx oc fetch` downloads the cloud copy to check what it holds. Report what is synced and whether you published; don't call the place saved or unsaved. Offer `rbx publish` when the user wants the changes live.

## Publishing places

```powershell
rbx publish --dry-run
rbx --place lobby publish
rbx publish --open-cloud
rbx publish --open-cloud --file build.rbxl --universe 123 --place-id 456
```

Publishing requires user authorization. Default: publish the selected Studio Edit
state to its existing place with the Studio login, without pushing files first.
Settle pending Live Sync with `lst --wait`. When the place refuses `SavePlaceAsync`
(Save Place API off, Team Create), Renium runs Studio's own Publish command and reads
the result from Studio's log (Windows and macOS). Don't change place settings or
credentials to work around a refusal without authorization.

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

## Report a Renium bug

Tell the user about any change you made to their place to get past a Renium failure.

```powershell
rbx report -m "what went wrong"
```

When Renium itself fails (an internal error, a crash, a sync that lost or mangled data), `report` writes `.renium/reports/<ID>/` and a zip beside it: the doctor checks, Studio status, the project file, the daemon log tail, recent crash reports, and the last `rbx` commands with their output from this project's Codex or Claude Code transcripts (`--since MINUTES`, default 120; `--no-transcript` leaves them out). Home paths and values after key, token or password words are masked. Tell the user the ID and path; they review the folder and attach the zip to the GitHub issue link the command prints.
