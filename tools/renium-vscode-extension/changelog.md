# Changelog

## Unreleased

### Bug fixes

- Pushes no longer crash Studio 0.742 (Windows exception c0000374). Renium hooks Studio's change-history recording for Terrain cancellations and token-based recordings; that hook was verified on 0.741 and corrupts 0.742's heap on the first recording that passes through it. The hook is now installed only on builds it is verified on (0.741 and older); newer builds skip it and cancellations restore Terrain explicitly. The first symptom was a crash during the automatic package desync, because package pushes are the common case that registers a recording before writing.
- `rbx pd` and `rbx pu` no longer fail with "Could not read Studio memory" on Studio 0.742, and the automatic package desync works there. Studio 0.742 moved the slot that holds a property's native binding on its reflection descriptors; the package path read the old slot. The binding is now located by its runtime type name, as the protected-property path already did.
- Package ownership checks ask Roblox for the account's group list once per Studio session instead of once per package. Places with many group packages tripped the rate limit, which left ownership unknown.
- `rbx lon`, `rbx lst` and the other Live Sync commands now honour `liveSync.initialSyncPriority` and `liveSync.initialConflictPreference` from the project configuration, and accept `--initial-sync-priority` and `--initial-conflict-preference` for one run. The CLI never sent these settings to the daemon, so `rbx cfg set liveSync.initialSyncPriority verify` had no effect from the terminal: the first connection still reconciled and pushed Studio-side differences into the place. Only the VS Code extension applied the setting.

## 0.4.8 - 2026-10-08

### Bug fixes

- `rbx lc` with its arguments in the wrong order now says what is wrong and prints the corrected command in one line, instead of a usage dump or sending the player index to Studio as the code: the player given before the code (`rbx lc --detach probe 1 CODE`), code passed to `--collect NAME PLAYER`, and code split into several unquoted arguments.
- The Studio bridge refuses WebSocket connections that carry a browser origin other than roblox.com (HTTP 403 at the upgrade). Browsers always send the page's origin, so a web page open on the same machine could previously connect to the local bridge ports and pose as a Studio plugin; Studio's own client sends no origin.
- Pushes that change files inside a Roblox package this account does not own are refused before Studio is touched, naming the package and its creator. Renium used to desync the foreign package on the user's behalf, and on Studio 0.742 that path took Studio down with heap corruption when a Creator Store package (TopbarPlus) had local edits. Revert the files or unlink the package in Studio to carry a copy.
- `rbx pf use` and `rbx pf adv` no longer fail when one of Studio's child processes refuses to join the performance job. Studio's WebView2 renderers are sandboxed and answer `AssignProcessToJobObject` with access denied, which aborted the whole profile ("Could not assign process N to performance job (Windows error 5)") whenever a WebView2 panel was open. Such children are skipped and listed as `skippedProcesses` in `rbx pf show`; the Studio process and every child that accepts the job are constrained as before.
- `rbx play -s` and `rbx play -s --players N` issued right after `rbx play -x`, or a few seconds after `rbx so` opened a place, no longer fail after a long wait with "server ready: false, clients connected: 0/2". Studio drops a test start while the previous test is still being torn down or the place is still settling. `rbx play -x` now returns only once the test's DataModels and windows are gone, and a start Studio drops is noticed and requested again; the result says so with `retriedAfterMs`.
- `rbx play -r` after a multi-client test restarts it with the same number of clients instead of in ordinary Play. Restart reuses the mode and player count the session was started with (`-p N` and `--mode` override them); a session started from Studio's own buttons restarts as ordinary Play, and `restartNote` says so.
- Studio test server and client windows left running after the Edit window that launched them was closed or restarted no longer block the next multi-client start. `rbx status` and `rbx cs` list them as `orphans`, `rbx play -s` closes those of its own place before starting and lists their process ids as `closedOrphans`, and `rbx play --kill-orphans` closes them all. Edit windows, and tests whose Edit window is still open, are never touched. A start that fails while such windows remain says so.

### Improvements

- `rbx q PLACE --props NAME,NAME` (or `--props all`) prints the saved values of those properties for every match, so settings such as `Workspace.PlayerCharacterDestroyBehavior` can be read from a closed `.rbxl`/`.rbxlx` without Studio or a full `rbx v --json` dump. NotScriptable properties are read like any other; enums print as `Enum.Type.Item` and references as paths. Names the file does not store are listed per match as `defaulted` (shown with the class default), `notSaved` (derived properties like `Position`) or `unknown`.
- `rbx play --add-players N` adds N clients to a running multi-client test and returns once they have connected, with their player names and the new client count; `rbx play --leave -p N` has the server kick client N's player, so `PlayerRemoving` fires as it does when a real player leaves, closes that client's window when Studio allows it (`windowClosed`), and returns once the server no longer lists the player, while the other clients keep running. Studio's `LeaveTest` alone closes the window but leaves the Player on the server, which stalled repeated join/leave cycles. Join and leave churn, such as hunting what keeps a departed Player alive, no longer needs a restarted session. Studio's limit of 8 clients applies, and the error says how many more fit.
- `rbx perf heap --server|--player N --out FILE` saves the play server's or a play client's Luau heap report, the data behind Studio's Luau Heap tool: memory categories, object tags, the per-script graph and the reference paths that keep unparented Instances alive. The result summarises the largest entries and the retained Instances, so finding what holds a departed Player no longer needs the Developer Console.
- `rbx oc universe` adds the experience's `upVotes` and `downVotes` from the public votes endpoint next to `playing` and `visits`, plus `updated`, the stamp of its last save or publish.
- `rbx publish --as`, `rbx publish --open-cloud` and `rbx oc place publish` report `live: true` once the place's version history lists the upload as the current published version, which is normally the first check right after the upload (`--wait-live SECONDS` bounds the wait on `publish`). `live: false` comes with the reason: the upload was stored as a save, or a later publish already replaced it. The version history's `publishStatus` is 1 for the version players currently get and 2 for versions a later publish replaced; an earlier build and its guide read 1 as "still processing", which sent an agent polling for a status that never comes on a version that was already live.
- "More than one Studio runtime matches this project" now lists the candidate Studios and says to pass `--place <placeId|window name>` before the command (`--runtime-id` never chose). The guides document the JSON shapes `l` returns (arrays, objects, `_truncated`, `_type` markers), the `--place` rule for two open Studios, `co --grep` being a regex, what `oc team remove-members` actually does (it revokes collaborator access for the whole universe), and the recipe for publishing while a collaborator occupies the live place.
- `rbx inp` gains `dismiss [X,Y]` and `press LABEL|X,Y` for Roblox's own prompts in play clients. A CoreGui prompt such as the one `AvatarEditorService:PromptSetFavorite` opens used to leave a client stuck: keys failed with "CoreGUI has keyboard focus", Escape was refused and clicks went to the game underneath. `dismiss` answers the prompt at the screen center with its own Cancel/No/Close/OK button, so the game receives the result, hides the prompt when that press fails, releases a focused Roblox text box, and reports `pressed`, `hidden`, `focusReleased` and `holdsKeyboard` under `coreGui`. Keys refused because of a Roblox prompt or menu now say to run it.

## 0.4.7 - 2026-10-05

### New

- `rbx wait` waits up to an hour (`-t`, default now 60 s) by re-issuing its check in slices under Studio's 120 s runner limit, and returns the final `value`; the timeout error names `-t` and `--detach`. `rbx play -s --until EXPR` waits for a server expression right after the session starts, and `rbx play -r` restarts a running session the same way it was started, so a code change no longer needs `play -x`, a sleep and `play -s`.
- `rbx l --server` runs on the play server only and fails when there is none instead of quietly running in the Edit window; `rbx l --edit` runs in the Edit window during Play. Luau errors now say where they ran (`[Edit window]`, `[play server]`, `[client 1]`), and a runner that failed because a module did not load carries `consoleErrors` with the lines Studio logged during the run.
- `rbx co --grep` is a case-insensitive regex (`-F` for plain text), `--level` takes `error`, `warn`, `info` or `output`, and both filters now search the whole retained console before applying `-n`; results report `scanned` and `matched`, and consecutive identical lines fold into one entry with `repeat`. `co` and `l` accept `-p` for `--player` like `inp`, `net` and `wait`.
- `rbx inp` lists its actions in `--help`, `wait` accepts `300ms` and `0.3s`, `hold KEY MS` presses a key for a duration in one action, keys pressed with `kd` stay held across later `inp` calls until `ku`, and results report `heldKeys` and `keysObserved`.
- `rbx play -s` reports `serverReady` once the play server answers Luau (it waits up to 20 s), `rbx status` carries a `liveSync` summary (pending count, paused/syncing flags, last error) while Live Sync runs, `rbx q` reports how many instances it `searched` and hints at `-s` when a name query finds nothing, and the "no connected Studio matches this project" error names `rbx ro`.
- `rbx oc CATEGORY --help` lists every action with its values, HTTP method and path, read from the route table; it marks paged and destructive actions (deletes, restarts, shutdowns, flushes, removals), says which flag fills `{universe}`, `{place}` and `{scope_id}`, and lists aliases.
- `rbx oc server list` without a VERSION shows the active servers and players of the newest place versions that have servers, grouped by version with each server's player count and a total; `rbx oc server find JOB` finds a server and its version; `--active` drops shut-down servers. `rbx oc server logs` takes `--grep TEXT` and `--severity error,warning,info,output` and labels each line `severityName`.
- On paged `rbx oc` actions, `-l N` is a total fetched 100 per request (`-l 1000` on server logs used to fail with "MaxPageSize must be between 1 and 100"), `--all` follows every page, `--pages N` stops after N, and results say `"more": true` when another page exists.
- Inside a single-place project, `rbx oc` commands use the place Studio is bound to (`.renium/studio-target.json`, written by `rbx so` and `rbx sx`) for `{universe}` and `{place}`, so `oc universe`, `oc place history`, `oc server list` and `oc fetch` no longer need `--universe` and `--place-id` on every call.
- `rbx oc universe` adds the live `playing` and `visits` counts from the public games endpoint, and a numeric global `--place` (`PLACEID` or `GAMEID:PLACEID`) works for `rbx oc` like `--place-id`.
- `rbx publish` and `rbx publish --as` check Studio before uploading: they refuse while Play runs in the selected Studio (`rbx play -x`; `--as` also accepts `--allow-play`), wait up to 20 s for a running Live Sync to settle, and refuse while file changes are pending or a conflict is open unless `--allow-pending` is passed. Results, dry runs included, report `liveSync` (`{"pending":N,"settled":true}`), `previousVersion` and the new `versionNumber`.
- `rbx status`, `rbx cs` and `rbx play -s` (single and `--players N`) give every Edit, play-server and play-client runtime its Studio `pid`; play start results also give the Edit window's `editRuntimeId` and `editPid`.
- `rbx ck` with no paths checks the `.luau`/`.lua` files that `git status` reports as modified, added, renamed or untracked under the project, and says so when the folder is not a git repository or nothing changed; given a folder it checks every Luau file under it instead of failing with "Access is denied".
- `rbx ps` reports what it did: created, replaced and deleted instances, created, updated and deleted scripts, property and attribute updates, and `verified` for whether Renium read the result back (always for a full push or one that includes a `.renium` store, otherwise with `--verify`). A push with nothing to send returns `"unchanged":true`.
- `rbx cmp BEFORE AFTER` takes the second place positionally, the same as `--against AFTER`; with `--full`, `ConfigureServerService` and `FilteredSelection`, which differ between saves of the same content, are skipped and listed under `engineManagedServices`.
- `rbx status` and `rbx doctor` include `"updateAvailable":"X"` when a newer release is known; on Windows, doctor adds an `rbxLauncher` check that warns when an `rbx.exe` on PATH runs a different Renium build.
- Luau errors containing "lacking capability Plugin" end with a hint to use `rbx perf start --player N` or `rbx perf micro-start`; "Luau runner timed out after" ends with a hint to pass `-t SECONDS` or use `--detach NAME` and `--collect NAME`.
- `rbx net show` returns `{"settings":{...},"active":bool,"preset":name|null}` instead of the `set` result, and the `poor` preset's jitter drops from 100 ms to 30 ms: Studio applies jitter cumulatively on a 20 Hz stream, and 100 ms left a client a full round behind the server.
- Luau results drop the per-call `path` and `runner` fields on success; errors are printed as one JSON line (`{"ok":false,"error":...}`) whenever stderr is not a terminal, so pipelines that read the last line still see the failure; tables with more than 128 string keys keep their object shape and carry `_truncated: {kept, total}` instead of turning into an entries list.

- Requests to different play runtimes run concurrently. The daemon used to run every Studio request one after another, so a five-second sampler on client 1 held up `rbx lc` and `rbx inp` for client 2 and for the server until it returned. Luau, console, input, UI, screenshot, wait and goto requests now take a gate for the one play server or client they address; the Edit window keeps the shared gate.
- `rbx lc --detach NAME CODE PLAYER` and `rbx l --detach NAME CODE` keep a Play runner alive after the command returns. The command comes back as soon as the code first yields, its threads keep running (`--lifetime` seconds, default 600) and record into a `state` table. `rbx lc --collect NAME PLAYER` reads that table together with the captured output, whether the code finished and its final results; `--stop` removes the runner. Before, threads started inside a runner died when the runner returned, so timing a one-second event needed a temporary LocalScript in the project.
- `rbx publish --as PLACE_ID` publishes the open Studio place to another place, as Studio's Publish As does: the place is serialized by Studio itself and uploaded through Open Cloud in one command, so a development place reaches its live place without publishing it first and downloading the version back. `--universe` names the destination's experience when it differs from the project's, `--key NAME` or `--key-env ENV` selects the key, and `--saved` stores a version without publishing it. Roblox refuses the upload with HTTP 409 while the destination is open in a Team Create session.

- `rbx gfx` shows and sets Studio's render quality without opening Studio settings: `rbx gfx` reports the edit and play levels (1-21, 0 = automatic) and the in-game slider; `rbx gfx max`, `rbx gfx auto` and `rbx gfx set --edit N --play N` change them for the Edit window or, with `--player N`, for one play client's process; `rbx gfx restore` puts back the values from before the first override, and the plugin restores them when it unloads. The guides tell agents to raise quality before visual captures and to record it next to performance numbers.
- Live Sync failures are recorded. Every error the Studio panel or the editor shows as "Sync failed" is now written to the daemon log, and `rbx lst` keeps `lastError` and `lastErrorAt` after the sync recovers, so a failure that clears itself can still be read afterwards.

### Bug fixes

- `rbx oc universe restart` and other actions with an empty response body return what ran (`{"ok":true,"action":"restart","universeId":123}`) instead of `{}`; `restart-servers` and camelCase action names are accepted; an unknown action fails at once with the list of valid actions before any key or network work; "unresolved placeholder" errors name the flag that fills it; Open Cloud reads retry 429 and 5xx answers up to three times honouring `Retry-After`.
- `rbx oc fetch` always reports `version`: without `--version` it downloads the newest published version (or the newest saved one when none of the recent versions is published), and `rbx oc fetch NAME` inside a project bound to another place fetches NAME instead of the project's place.
- When Open Cloud refuses a publish upload with HTTP 409 ("Server is busy"), the error says what blocks it: the destination's Team Create members and any saves newer than the last published version, with a code (`team_create_active`, `unpublished_newer_save` or `place_busy`) and a hint; when neither lookup works it falls back to the original reply plus a Team Create hint.
- `rbx status` no longer hangs on a busy daemon: after about 12 s it says the daemon is busy, which operations it is serving and for how long, and points to `rbx dm status` and `rbx report`.
- `rbx lst --wait` no longer reports an unfinished sync when nothing is pending and no pass is running; otherwise it names what is still open (pending paths, a running pass, a conflict, paused file sync, unpulled Studio changes, or the current error) plus the last recorded error.
- `rbx` works in Git Bash on Windows: the installer, `rbx upd`, the Windows release archive and the VS Code extension bundle ship the extensionless POSIX `rbx` script next to `rbx.cmd`, including the stable copy in `~/.renium/bin`; the script looks for `renium.exe` explicitly and falls back to `%LOCALAPPDATA%\Renium\bin\renium.exe`.
- An outdated RENIUM.md no longer fails the command: the guides refresh whether the CLI runs as `rbx` or `renium`, the command still runs, and stderr gets one line saying the guide was updated.
- The "update available" banner no longer appears on every call. It prints only when stderr is a terminal, at most once an hour per installation; piped output stays clean JSON.
- Live Sync no longer reports "Sync failed" when a collaborator changes the place while a sync pass is staged. In a Team Create session, an export that found Studio changed under it ("Studio changed Workspace while native sync was staged" or "during native export") was treated as a failure: the panel showed Sync failed, the retry delay grew to 30 seconds, and the next collaborator edit started it over, so busy sessions showed the failure constantly even though every pass eventually synced. Such races are now retried quietly after half a second, like project files changing during a pass; only a persistent failure is reported. The same race during the Live Sync restore that follows a daemon restart used to leave Live Sync off until `rbx lon`; the restore now retries too.
- `rbx co --server` and `rbx perf --server` read the play server only. Before, when the server's bridge was not connected yet, or a request had been routed before it connected, the daemon quietly answered from the Edit window, whose console holds only the lines from launch, so later server output never appeared and an agent concluded the game had stopped logging. An explicit server request now waits for the play server and fails with the connected bridges listed when there is none.
- `rbx oc place publish FILE` no longer fails with "Invalid version type": the request defaults to `versionType=Published`, and `-q versionType=Saved` still saves without publishing. `rbx publish --open-cloud` accepts a stored key with `--key NAME` like every `rbx oc` command, not only `--key-env`.
- `rbx oc fetch` without `--version` now downloads the newest published version by number, read from the place's history, instead of the delivery copy, which could still serve the previous version right after a publish; the result reports the version. When the key cannot read the history, the delivery copy is used as before.

## 0.4.6 - 2026-09-30

### Improvements

- Pulls and pushes of large places are faster. A pull copies the existing project into its stage while Studio serializes and discards the stage without waiting for the delete, which antivirus scanning had stretched to about five seconds; a repeat pull of a 79,000-instance place went from about 6.6 s to about 1 s. A push prefetches its changed files in parallel, looks up scopes by ancestor instead of scanning every scope per path, reads snapshot files in parallel, keeps every service store cached and fetches live sources in byte-bounded batches across the bridge channels; a no-op push of that place went from about 700 ms to about 300 ms and its editor commit from about 900 ms to about 100 ms.
- A full push of a place with more than 5,000 models no longer fails at transaction begin with "post-commit property changes must be a bounded array", and paths with empty instance names, which Roblox allows, are accepted.
- `rbx perf micro-stop` and `rbx perf micro` now report `frames`, the number of complete frames the saved dump holds, with a note when it is zero. Studio keeps a fixed amount of profiler log per thread and drops frames whose data was overwritten, so a busy client keeps far fewer than the frame limit and a heavy game phase can leave none; an agent used to learn that only from `rbx perf analyze`, which now explains it as well instead of claiming collection was off. The performance guide describes the limit.

### Bug fixes

- A place opened by Renium no longer sits behind Studio 0.741's auto-recovery prompt, which one of Studio's own plugins draws where no automation can press its buttons. Before launching or reopening Studio, the place's auto-recovery files are moved to AutoSaves/Archived under the names Studio's own Ignore gives them, so they stay recoverable, and the daemon log says how many were set aside.
- The package-modification notice Studio can raise while a place loads, before any push is watching for it, is now accepted on Windows and macOS, so the plugin connects instead of `rbx status` timing out behind it.
- `rbx status`, `rbx ro` and every Studio command now say when a Studio window hung at launch instead of waiting for it, read from Studio's own hang monitor in that window's log. Studio 0.741 does this to windows opened while another Studio window is running, mostly before the automatic sign-in finishes, so the window sits at "Roblox Studio" unresponsive and never opens its place or connects. The verdict names the pid and the stage, says to close the window and open the place again, and to close the other Studio windows first or open the place from the running Studio if it repeats.
- `rbx cmp`, `rbx so` and every other offline place read now open a place by its content, as Studio does, so an .rbxl file that holds XML (saved by a tool that keeps the name) or an .rbxlx that holds binary no longer fails with "Invalid file header". The name still decides the format of a place Renium writes.
- A push no longer fails verification with "CFrame was not retained" over a difference of a few hundred-thousandths of a stud on a part inside a welded assembly. Studio re-derives such a part's CFrame from the assembly root through the weld offsets once the tree is in the place; verification now allows a thousandth of a stud and a ten-thousandth in rotation, and still reports a part that landed anywhere else.
- A push no longer fails verification with "Velocity was not retained" when the physics engine recomputes a part's velocity after the tree is in the place, which it does for every part inside a welded assembly. Velocity, RotVelocity and the assembly velocities are engine state during verification; the files keep their saved values.
- A push no longer fails verification with "TexturePack was not retained" when Studio issues a different texture pack asset for a SurfaceAppearance, Decal, MaterialVariant or TerrainDetail than the one in the files. Studio derives that pack from the appearance's maps and can replace the id, for example when the files came from another place; verification now accepts whichever pack Studio holds and the next pull records it.
- Studio 0.741 connects again on Windows. Its build compares the package-popup flag with different instructions than 0.740, and Renium's scan for the notice patch recognised only the old one, so every Edit connection was dropped with "Studio package popup flag consumer is not unique" and the plugin reconnected in a loop until `rbx status` timed out. The scan now recognises every form the compiler uses for that comparison, and a Studio build the scan does not recognise no longer keeps Studio from connecting: the patch is skipped with a warning and package pushes keep using the dialog watcher.
- `rbx pf off` no longer fails with "Could not restore Studio processor affinity (Windows error 5)" and stays in `cleanup-pending` after a Play-session Studio exits. Windows keeps an exited process openable while another process holds a handle to it, and Renium's process identity check only compared the start time, so the exited Studio still counted as enrolled. Every identity check now treats an exited process as gone, which also drops it from `rbx pf show`.
- `rbx pf adv` and `rbx pf use` no longer refuse CPU, core and priority limits with "Profile memory limits need … more bytes than the protected system commit budget" on a machine whose commit charge is near its limit. The budget check now runs only when a memory cap would raise Studio's commit above its current usage; limits that add no commit apply as before.
- On Windows, `rbx ky` and `rbx ty` now use the game's virtual input like every other input command, so they report `inputMethod: "virtual"` and touch neither the cursor nor window focus. 0.4.5 had moved the pointer commands but left key presses and typing on the window path, which contradicted the input guide and made an agent stop a test to report it. A key can now be held for up to 60 seconds; the old limit was 2 seconds, and a longer request was silently shortened.

## 0.4.5 - 2026-09-29

### New

- On Windows, `rbx inp`, `rbx clk` and `rbx pr --world` now deliver mouse input through Roblox's own virtual input instead of posting messages to the Studio window. A move fires `MouseEnter`, `MouseMoved` and `InputChanged`, `Mouse.Target` follows the injected position, and ClickDetectors respond to an injected click, none of which the window-level path could do. The physical cursor and window focus are never touched, so the Play window no longer needs to be located, calibrated or shielded. Set `RENIUM_INPUT_OS=1` to get the old path back.
- `rbx inp`, `rbx clk` and `rbx pr` no longer fail with "hits CoreGUI" when a Roblox overlay such as the chat window or the player list covers the target. The plugin hides those overlays for the event, as a tester closing the chat would, and restores them when the input batch ends. A point under a top bar icon is delivered too, with that element hidden for the moment, and the result then carries `throughSystemUi` so an agent knows a player could not have clicked there. The Roblox menu button's corner is reserved by the engine itself and stays refused, and the error now says so.
- `rbx oc fetch --version N` downloads a saved version of the place from Roblox's version history (`rbx oc place history` lists them), so an agent asked about an older state can compare real history instead of guessing that stray place files on the machine are earlier versions.
- `rbx rev --path FILE --print` writes a file's saved history copy to stdout instead of restoring it, so an agent can diff or copy an older version without undoing later edits. Pass a `.renium/editor-history/<id>` entry as `--path` to pick an older copy.
### Improvements

- `rbx status` now reports `teamCreate` and `placeVersion` for the selected Edit place. `teamCreate` is true when the place is in a Team Create session, where Studio saves every edit to Roblox by itself, and `placeVersion` is the place's saved version number. Agents used to tell users their synced work was "not saved" without any way to check, so the agent guides now tell them to report work as synced and to read these fields instead of guessing.
- The agent guides now explain how saving and publishing differ from syncing, that an agent can reopen a closed bound place with `rbx ro` instead of stopping, and that a reopened cloud place should be compared in verify mode before Live Sync merges it. They also give the real filter syntax for `rbx bb` searches (`is:Class`, `tag:Name`, `Prop=value`) and say that an empty `rbx co` result means there are no messages. They also cover Windows PowerShell 5.1 quoting, MicroProfiler capture windows, keeping the capture id, and Creator Store inserts becoming project content under Live Sync.
- `rbx status` now waits while a Studio that was just launched is still opening its place. Right after `rbx ro`, it used to report at once that Studio had a different place open, because another Studio was connected while the launched one was still loading, and its verdict told the agent to run `rbx ro` again. The verdict now names the starting Studio and how long ago it launched, and `status -w SECONDS` keeps waiting for it up to that long. A closed Studio still reports immediately.
- `rbx go PATH` now walks to the nearest point of the target part's or model's bounding box instead of its pivot. A wide or tall model such as a tree has its pivot inside the canopy or trunk, so the character could not get within eight studs of it and `go` reported a failure after its timeout. `go --pos` still targets the exact point.
- The lifecycle guide now tells agents that when a collaborator in Studio still sees old behaviour on a Team Create place, the code is the suspect rather than the sync, and that `rbx oc fetch` downloads the cloud copy to check what it holds. An agent had told a user that a tester in Studio needed a publish.
- The agent guides now say that Play runs the game's real server code against the signed-in account's live data when Studio API access is on, and that agents should not make backup copies of a project because `rbx rev` and Studio undo already cover reverts. They also tell agents never to hand the user steps they could run themselves, such as how to undo their own work, and to do them or offer to instead. The playtest guide now says that console stacks naming `cloud_<id>` or `user_<name>` scripts come from installed Studio plugins, not the game.

### Bug fixes

- `rbx upd` no longer gets stuck on the old CLI version after Cursor or VS Code has already installed a Renium extension update since the editor was opened. The editor refused the next update with "Please restart VS Code before reinstalling Renium", and `rbx upd` treated that as fatal, failed to roll back, and then failed the same way every time after that. It now finishes the CLI and plugin update and tells you to restart the editor and then run `rbx upd apply --component extension`.
- `rbx upd` no longer fails on Windows with "Access is denied" while replacing the CLI's folder. Renium's audio helper processes, which the daemon starts from that folder, were still running and kept it locked. The updater now stops them before it swaps the folder or restores the old one, and the daemon starts them again on its own.
- `rbx l -`, `rbx lc -` and `rbx ck -` no longer fail with "got Unicode character U+feff" when the piped code starts with a UTF-8 byte order mark, which PowerShell adds to text it pipes. The mark is now ignored, as `rbx bb -J -` already did.
- `rbx go --pos X,Y,Z` no longer rejects a negative first coordinate such as `-330,300,1485` as an unknown flag, and the same fix applies to the model generator's `--size`. A value that starts with a minus sign used to be read as a flag.
- When Studio has a different place open than the one the project is bound to, the `rbx status` verdict now says that `rbx ro` opens the bound place, instead of only telling you to "open that place".
- `rbx cmp --full` no longer reports differences that no edit made. Comparing a saved place with a project listed hundreds of them: the cached collision data Studio recomputes for every mesh, weld constraint state, the deprecated collision group id Studio derives from the group name, an explicit `InputSink` value equal to its default (the reflection database knows that property only under its serialized name), `Lighting.Technology` on a place Studio had already moved to Unified lighting (Studio keeps the old value in the file for rollback and shows Unified either way), engine `RBX_` migration attributes, hidden fields such as `TextChatService.IsLegacyChatDisabled` that a project never captures, and instances with `Archivable` off, which a saved place never contains. The result now skips all of these and counts them under `engineManagedProperties`, `notCapturedProjectProperties` and `unsavedProjectInstances`, so real changes such as script edits, collision groups and `Workspace.SignalBehavior` stand out.
- Live Sync now starts when the project files hold a rig or another model with constraints that Studio does not have yet. Pushing such a tree made the engine settle its constraint attachments after insertion, the change tracker counted those writes as Studio edits made during the import, and every attempt ended with "Studio changed Workspace while native import was staged; retry the sync". Writes to a tree the import itself inserted are no longer counted.
- `rbx pl` and Live Sync no longer fail with "Studio native Workspace snapshot contains N instances; expected N+1" on places with an R15 rig. Studio adds a `Status` object under such a rig's Humanoid and never saves it, whatever its `Archivable` value, so Renium counted one instance more than Studio serialized. Renium now leaves `Status` objects out of exports and change tracking, as it already did for touch transmitters. Agents used to work around this by deleting the object from the user's place.

## 0.3.15 - 2026-09-28

### Improvements

- `rbx l` now loads a ModuleScript's current source when you run it with Studio stopped. A module that changed since an earlier run, through Live Sync or an edit in Studio, returns its new version, not the version from the first run that required it. Edit-mode code runs inside Renium's plugin, which stays loaded between runs, and Roblox keeps each `require` result for as long as the plugin is loaded. Each run now has its own `require`. A module required twice in one run still returns one table, but nothing carries over to the next run. Cyclic requires and modules that return nothing raise the same errors as in Roblox, and requiring by asset ID still goes through Roblox. During Play, `rbx l` and `rbx lc` still use the running game's own `require`, so they share the modules the game has already loaded.

### Bug fixes

- Resizing the Renium widget around the width where its settings rows switch between side-by-side and stacked no longer fills Studio's Output with "Maximum event re-entrancy depth exceeded for Object.AbsoluteSizeChanged" or makes the rows flicker. Switching layout changed the rows' height, which showed or hid the list's scrollbar and changed their width again. The widget now switches layout at most once per frame, and a stacked row needs 12 extra pixels of width before it goes back to side-by-side.

## 0.3.14 - 2026-09-27

### Improvements

- The daemon now gives memory back to the system about a second after a pull, push or Live Sync cycle finishes, so after pulling a large place it sits idle at about 70 MB instead of about 290 MB and no longer grows with repeated pulls. The daemon's worker threads used to keep memory that had already been freed until they ran again, which could take indefinitely while the daemon was idle.
- Five minutes after the last pull, push or Live Sync activity, the daemon now also drops the caches it keeps to make back-to-back syncs fast. After a Live Sync push of a large place, idle memory falls from about 190 MB to about 75 MB. Most of that memory is the parsed copy of the project's `.renium` store files. The other caches are the project layout and the copy of the last Studio export. The first push or pull after a break like this reads the stores again, which takes well under a second, and a pull transfers unchanged services from Studio again.

### Bug fixes

- `rbx pl` and the first comparison when Live Sync starts no longer hang forever on computers with a single CPU core, such as small cloud VMs and some containers, or when `RAYON_NUM_THREADS` limits the daemon to one worker thread. The pull kept its only worker thread busy waiting for results that needed that same thread to run, so it never finished and showed no error. It now completes with any number of threads.
- The extension on Intel Macs and Linux ARM64 uses its bundled `rbx` again, so it no longer needs a separately installed Renium CLI to run commands. The 0.3.13 packages for those two platforms put `rbx` in the folder for a different processor, so the extension never found it. Windows, Linux x64 and Apple Silicon packages were not affected.

## 0.3.13 - 2026-09-27

### New

- `rbx report -m "what went wrong"` compiles a bug report for the Renium developer: the doctor checks, Studio status, the project file, the daemon log tail, recent crash reports and the last `rbx` commands with their output taken from the project's Codex or Claude Code transcripts, written to `.renium/reports/<ID>/` plus a zip, with home paths and key, token or password values masked. It prints the report ID and a GitHub issue link to attach the zip to. An agent that hits a Renium failure is told to run it and hand the user the ID.
- The daemon keeps a rolling log (`logs/daemon.log` beside its discovery file, 8 MB with one rotation) with one line per request, its duration and outcome, so a report shows what Renium did before a problem.

### Improvements

- `rbx access read TARGET PROPERTY [TARGET ...]` reads one property from several instances in one call and under one approval, up to 64 per call, and returns a `results` list with each path, class name and value. Reading the Status of every PackageLink in a place used to take a separate `read` and `approve` for each package.
- `rbx access` describes every argument in `--help`; TARGET, PROPERTY, `--ords`, VALUE, FUNCTION, ARGUMENTS and REQUEST_ID used to be blank.
- `rbx bg SERVICE TARGET PROPERTY` takes the property as a plain word after the target; `-p PROPERTY` still works, and with a flag selector such as `-i ID` or `-n NAME` the one free word is the property. A property written without `-p` used to be rejected, for example with "the following required arguments were not provided: --property".
- `rbx pp` and `rbx pu` wait up to 120 seconds by default instead of 20, and `--timeout` accepts up to 600 seconds. Publishing a large package used to fail every time with "Package operation did not finish before the deadline" or "Studio helper exceeded its 19989ms deadline", and `--timeout 60` was rejected with "Package timeout must be >0 and <=20s".
- `rbx init` in a folder that holds a Rojo project converts its `*.project.json` into `renium.project.jsonc` instead of writing a starter project beside it, so a Rojo project moves over with `rbx init` and `rbx lon`. `rbx ir` takes the Rojo file or folder as its argument (`rbx ir --preview`, `rbx ir default.project.json --apply`), writes the agent guides together with the project, and reports an existing identical file as unchanged instead of demanding `--force`. `rbx ir --project FILE` used to stop before converting anything.

### Bug fixes

- A project tree node named `StarterPlayerScripts` or `StarterCharacterScripts` under `StarterPlayer` builds that class when it declares no `$className`, as it does in Rojo, instead of becoming a Folder that Studio ignores; converted Rojo projects also carry the class explicitly.
- `rbx ro` and `rbx so` no longer fail on Windows with "Could not protect Studio activation: A dynamic link library (DLL) initialization routine failed. (os error 1114)" when Studio is still running its loader at launch. The launch guard retries a module snapshot that Windows briefly reports as unreadable, recognizes its own image under another spelling of the same path, and repeats the protection call instead of giving up on the first transient failure.

## 0.3.12 - 2026-09-26

### Bug fixes

- Live collaboration no longer loses a file saved while a change from another participant arrives for it. The mirror used to write the remote content over the pending save; it now merges the save with the remote change line by line (a line both sides changed keeps the local version) and shares the merged result.
- A collaboration participant can no longer write outside the shared project: a document key that leaves the project folder (`../x`), names another drive, or targets `.git` or `.renium` is refused and logged instead of written.
- Saving a file the project does not share (such as an `.env` in the project folder) no longer sends it to collaborators; only files under the configured project inputs and the recognized root files travel.
- A text-like file that is not valid UTF-8 (for example a Latin-1 `.txt`) is shared with its original bytes instead of arriving empty.
- `rbx publish` through Studio's own Publish command reads the result only from the log of the Studio it triggered, so a second open Studio publishing at the same time can no longer be reported as this place's success or failure.
- Writing a place or model no longer fails with "Property type mismatch: Expected Workspace.SimulationRate to be of type Float32, but it was of type Float64" (or any other number stored wider or narrower than the reflection database declares, as Studio does for several properties). Every export, build, publish and native import now converts such values to the declared width, so a single property can no longer abort the whole file.

## 0.3.11 - 2026-09-25

### New

- `rbx oc key add NAME` stores an Open Cloud API key once per user, read from a hidden prompt and kept DPAPI-protected on Windows or in the Keychain on macOS; every `rbx oc` command uses the stored default when the environment variable is unset, `--key NAME` picks another, and `key list`, `key use` and `key remove` manage them. The key never appears in arguments, project files or listings.
- `rbx oc games [NAME]` lists the experiences a key can reach (its scoped universes plus the public experiences of the key's user and their groups) and finds one by name ignoring case, emoji and punctuation; `rbx oc fetch NAME -r DIR` downloads its root place and imports it into a project in one step.
- `rbx lst --details` and `rbx lon --details` list every first-connection conflict with the properties that differ and both values instead of three names and a count.
- Mute or unmute Studio audio, or opt into muting only while its window is unfocused, from `rbx audio` or the editor's Studio Audio menu. A persistent global setting covers current and newly opened windows and is editable by agents. Project sounds and volume levels are unchanged.
- Publish places from Studio with `rbx publish`, or build and upload project files with `--open-cloud`; the editor includes a publish preview and confirmation.
- Agents find Renium after a fresh install: `rbx setup` (run by the installer) writes a short note into the global instruction files of installed agents (Claude Code, Codex, Gemini CLI, OpenCode, Windsurf) that explains `rbx` and points to `rbx init`; `rbx --help` opens with the same hint, and the editor extension offers to initialize a folder that has no project instead of creating one silently.
- Approved protected Studio function calls, including ordered batches of up to 32 calls with exact approvals and partial-failure results.

### Improvements

- `rbx status` names a Studio that could not open its place and gives Studio's reason (for example, the signed-in account cannot edit it) instead of reporting that no Renium plugin connected.
- `rbx plugin process PID` also reports whether the process is Roblox Studio, when it started and why it could not open its place; `--terminate-studio --started-unix TIME` closes a Studio a plugin launched and refuses any other process that reuses its PID.
- The bundled Roblox reflection database now matches Studio 0.740: 32 new classes (Path3D, ViewportCamera, AnimatedImage, MemoryStoreDistributedCounter, ...) and enums such as `DistanceAttenuationMode` are known, so `rbx cmp` and captures name them instead of failing or storing numbers; a store written with an enum number still compares equal to the named value.
- `rbx publish` from Studio runs Studio's own Publish to Roblox command when the place refuses `SavePlaceAsync` (no Save Place API), waits for Studio to log the result and reports the new version; it no longer needs a stored API key or manual publishing for such places.
- The Studio plugin's sources are `.luau` files.
- Native function discovery follows Studio's reflection dispatch and receiver relationships across relocated layouts, with verification on Windows and Apple Silicon.

### Bug fixes

- Store commands such as `rbx bem` no longer fail with "Failed to stat ...\src\<Service>\__roblox_sync_settings.renium" when the project folder or working directory is given as a Windows short path (`rbx -r C:\Users\SUPERW~1\...`); the short path is expanded to the long form the project is registered under, so the store in `instances/` is found.
- Several agents or editors working in one project while Studio also changes no longer get "changed on both sides" reviews for files only one of them edited. A reconcile that pushed to Studio and then failed used to leave its own push looking like a Studio edit, and the next reconcile reported every pushed file as a conflict.
- Files edited while Live Sync reconciled now reach Studio. The reconcile used to record them as already synchronized, so those edits stayed only in the files.
- Live Sync keeps working while files change continuously. A reconcile or Studio pull that races a file edit retries quietly instead of failing with "Project files changed while Studio export was running", and a store Studio changed is merged with edits made to other instances in it meanwhile.
- `rbx bs` and `rbx ba -p` store composite property values in their typed form, so `Size = {"x":6,"y":1,"z":6}` becomes a Vector3; a value that is not the property's type is rejected. An untyped value used to be stored as a table and stop every later push with "Vector3 expected, got table".
- A push into a place whose service roots lack the project's attributes (for example a fresh or empty place) no longer fails with "Studio did not retain native supplemental properties"; service-root attributes are now written after the native import, which applies only their properties.
- Commands run by a plugin that holds a Studio place lease no longer fail with "The running daemon did not confirm exclusive Studio ownership"; the daemon now confirms the lease after verifying it against the place.
- `rbx oc --key-env NAME` no longer falls back to the stored default key when `NAME` is unset; only the default `ROBLOX_API_KEY` does, so a caller that names its own key never runs with another one.
- A push that adds an instance Studio refuses to create for plugin code (such as `Noise`) now names the instance and the reason instead of "Native insertion is missing an expected root".
- Saved fields that no plugin API exposes (`Lighting.Technology`, `Workspace.StreamingMinRadius`, `Players.BanningEnabled`, `ServerScriptService.LoadStringEnabled`, `Terrain.GrassLength`, `Path2D.Transparency`, ...) now sync both ways: native captures carry them, so pulls and Live Sync reconciles compare them and correct a stale file value, and a push writes a changed one through the native property writer without an approval, as these are ordinary settings a user can change in Studio. A field that still cannot be written is returned as `unsupportedProperties` with the reason and a warning on stderr, instead of being dropped silently or failing the push as "not retained".
- The daemon no longer deadlocks when a pull, push or property command arrives while Live Sync is applying a Studio change; every command on that daemon, including Luau runs on another Studio, used to hang until the daemon was restarted.
- `rbx l` gives up with a timeout error once its own limit plus the Studio connection wait has passed instead of waiting up to thirty minutes for the daemon.
- AudioEmitter and AudioListener custom attenuation curves (`DistanceAttenuation` and `AngleAttenuation`) now sync: a push applies them through the engine's curve setters and a pull reads them back, instead of silently skipping them as unreadable while the files kept the curves.
- A deprecated property that Studio no longer reports (such as `CollisionGroupId`) no longer fails a push or a Live Sync start as "missing from Studio" when the project files still carry it.
- Studio audio control and other per-process features no longer fail with "Could not read the executable path" on Studio windows whose image Windows cannot express as a drive-letter path; the native path is used and mapped back to a drive letter.
- A push no longer reports a property Roblox never saves (such as `VRService.GuiInputUserCFrame`) as "not retained" when the project files lack it.
- Pushing a place whose MeshParts share names no longer stops with "Mesh geometry target path changed" when Studio orders those siblings differently from the files; the native geometry write now targets the part by its live position.
- Auto audio always unmutes the focused Studio window, including after reopening or controller restarts. Windows suppression no longer leaves a persistent mixer mute behind.
- A script whose files hold the legacy `Disabled` flag no longer conflicts with Studio's `Enabled`, and a property the reflection database does not know yet no longer conflicts when only Studio reports it; a first connection that differed only in those ways starts without review.
- On macOS, a Studio that Renium launched no longer locks, warps or hides the pointer while it is not the active application. A Play session that captures the mouse takes it only once the user switches to Studio and releases it when they switch away.
- Live Sync retries a reconcile that failed because files changed while Studio was exporting, with backoff, instead of staying on "Sync failed" until the next file edit; a manual retry also triggers it.
- A command run outside a project no longer creates a project file and agent guides in that folder; it reports that no project was found and how to create one. Only `rbx init` and an explicit `-r DIR` create projects.
- Terrain Live Sync stays attached to the correct Edit session during Play and automatically recovers replaced relays and failed attachments, including after daemon or helper updates.

## 0.3.10 - 2026-09-20

### New

- Every store command takes its target the same way: a positional name or dotted path (`rbx bs Workspace Lobby.Door -p Anchored --bool true`, `rbx br Workspace Lobby.OldPart`), `-i ID`, `-x INDEX`, `-n NAME` or `-c CLASS`. `mv`, `cp`, `rn`, `rm`, `upl`, `mep` and every parent option (`-I`) accept a dotted path where they took only a settings ID.
- `rbx bg` and `rbx bs` with no target address the service itself, so `rbx bg Workspace -p Gravity` works like `rbx in Workspace`.
- `rbx f` scopes a search to one subtree with `--path` or `-I`; `-I` takes an ID or a dotted path.
- Native mesh writes: after a MeshId is written directly, Renium cooks the mesh so the part renders at its saved size instead of the mesh's native scale.
- A `fast` Cargo profile (`cargo build --profile fast`) builds without link-time optimization in less than half the time of a release build.

### Improvements

- Text output is cheaper for agents: paths print as one dotted string with `[n]` ordinals (the same form path arguments accept), ambiguous-match lists share one path and repeat only id and ordinal, single-precision numbers print short, full records hide engine-recomputed properties unless `-F props` asks for them, console entries drop the unix stamp and use print/info/warn/error, script grep groups hits by file, and `rbx clients` drops channels and ports.
- Which properties take part in a comparison is decided in one place for equivalence checks, retention reports and merges, so a value never counts as equal in one and different in another.
- The reconcile engine is split into modules by concern.

### Bug fixes

- A Studio launched in the background stays in front once the user selects it, even while it is still settling.
- The viewport camera can move while a native sync is staged; Workspace.Camera changes no longer abort the sync.
- Live Sync starts when Studio and the files hold the same new instances in a different order; duplicate new instances pair by their data, and only a genuinely ambiguous pairing is reported.
- Merges no longer duplicate an instance both branches added under the same id, and engine-recomputed properties (VertexCount, Unscaled sizes, CFrame0, Decal ColorMapContent, RBX_ attributes) never conflict.
- Native root writes locate their instance by live path, and Roblox-internal attributes are left to Studio.
- File-only property names (WorldPivotData, InitialSize, FluidFidelityInternal) compare under their logical names, so a model import no longer reports false differences.
- ScrollingFrame.CanvasPosition is treated as engine state; the Studio reopen target survives daemon restarts; a root place file is preferred.
- Guide examples and the CLI accept `-r` before the command name, and the guides are embedded in the executable so a stale copy on disk cannot block commands.
- The build cache stays small: line-table debug info only, no debug info for dependencies, no incremental release cache.

## 0.3.9 - 2026-09-17

### New

- Live collaboration: `rbx collab start` shares a project's files as one live document and prints an invite; `rbx collab join` fills a folder from it. The room runs on the host through a Cloudflare quick tunnel, or on a relay that keeps the room and its history. `rbx collab relay deploy` publishes that relay to a free Cloudflare account in one command and makes it the default.
- The VS Code extension gained a Collaboration view with participants and the invite link, Start, Join, Leave and Copy Invite commands, a status bar item, and other participants' cursors and selections drawn in open editors.
- When no Studio runtime connects, the error and `rbx status` now say why: Studio closed, plugin missing, plugin needs a Studio restart, plugin not connecting, or a different place open.

### Improvements

- The Studio panel lays itself out by its size: a single row with icon buttons when small, the card and header when there is room, and settings rows stack when the settings window is narrow.
- Connect, Disconnect and Cancel are one power button whose color carries the state.
- Building from source now requires the current stable Rust; the old-toolchain check was removed.

### Bug fixes

- The Studio plugin could wait forever for the place to report itself loaded and never show its toolbar or connect. It now starts once the place settles.
- Studio process ages are read correctly on macOS.

## 0.3.8 - 2026-09-17

### New

- `--name` and `--class-name` combine into one selector in store commands.
- `rbx bb -J` accepts inline JSON as well as a file or stdin.
- Pulling or importing into a folder that does not exist yet creates it.

### Improvements

- Live Sync keeps working on Studio builds where the Terrain undo hook cannot be installed (Studio 0.739 and later). Cancelled recordings restore Terrain explicitly after Studio's own undo, so correctness no longer depends on that hook.
- Multiplayer tests keep waiting while the server and clients are still connecting, and close test windows that never connected.
- The agent guide tells agents to fix and rerun malformed commands, read each guide once per session, keep query results small, and fix defects they find instead of reporting them.

### Bug fixes

- Stores written by `rbx pi` no longer make Live Sync re-apply every mesh, fail verification on fields Studio never exposes, or report differences for CollisionFidelity and ClockTime the store never held.
- A failed Live Sync restore no longer replays on every command; the status reports the error once until the next `lon`, `rp` or `dp`.
- Long pushes no longer lose their Studio transaction while native writes run, and native MeshId and SourceAssetId values Studio already holds are not rewritten.
- Identical sibling parts no longer fail with "Could not uniquely identify".
- Place and model exports keep scripts disabled, and place and model imports read `Disabled` back.
- `rbx ba --no-parent` on a populated store adds the instance under the service root, as documented.

## 0.3.7 - 2026-09-15

### New

- `rbx pi` imports a saved RBXL/RBXLX place into project files without Studio.
- `rbx bep --base ORIGINAL.rbxl` rebuilds a place on top of its original file, keeping every service the project does not sync.
- Pulling or importing into an empty folder with `-r` creates that folder's own project instead of writing into a parent project.
- Every CLI argument is described in command help, and help is shorter.
- CLI output is compact by default; Luau string-keyed tables come back as JSON objects and error traces are trimmed.

### Improvements

- The daemon returns freed memory to the OS promptly. Heavy push and pull sessions now hold about 300 MB instead of 850 MB.
- Pushes skip work Studio already reflects, and the verified push proof survives while Live Sync is running.
- Full pushes no longer reset properties Studio cannot reset.
- Studio launched by Renium keeps its remembered window size and maximized state without taking focus.
- Undoing a sync keeps your selection instead of selecting the restored instances.
- On macOS, Studio's package modification notice is disabled when the plugin connects.
- Every privileged daemon request must be signed.
- The snapshot workflow, undocumented commands, hidden tuning knobs and pre-0.3.2 migrations are removed.

### Bug fixes

- Pulls no longer fail with "Unsupported attribute binary value" on BrickColor, Font, ColorSequence and NumberSequence attributes, and pushes accept those attributes again.
- Targeted pushes (`ps -i`) remove attributes deleted from the store and never assign one settings id to two instances.
- Instances inserted natively under a service keep their project identity, so renaming or moving them by id no longer duplicates them.
- Filtered pushes record native insertions without change tracking, fixing "Native insertion is missing an expected root", and failed import sessions are released on rollback instead of hitting "Too many active native import sessions".
- Whole-service upserts (`ps -u`) send values only for project-created instances, so they no longer take minutes on large services.
- Instances moved out of a subtree that is deleted in the same push are moved before the delete.
- `cp` clones script sources, `rm -R` keeps descendants, `cr` stores property names and qualified references, `ip` picks unique init stems and accepts dotted imports.
- `cmp` ignores the viewport camera placement.
- Model imports accept typed numbers, write scripts under the source root and store scalars the way pulls do.
- macOS package pushes no longer resolve the wrong sibling.
- Same-named new roots import natively, and pushes into an emptied place succeed.
- Legacy migrated properties, engine migration flags and forced text wrapping compare correctly during verification.
- Native import tag announcements and mesh re-applies stay out of the change journal.

## 0.3.6 - 2026-09-13

- Windows package warnings no longer interrupt Renium; package links remain intact.
- Studio opens without stealing focus and can still be brought forward manually.
- More reliable screenshots, place targeting and Explorer property editing.
- Improved sync and import fidelity for Lighting, models, constraints, attributes and references.
- Rollback no longer moves objects with missing parents to service roots. Internal recovery errors stay out of Studio Output.
- Per-client network delay and jitter support up to 1000ms on compatible Studio builds.

## 0.3.5 - 2026-09-12

### Sync and Studio

- Faster pushes for large places, small changes, and projects that haven't changed.
- Terrain sync preserves materials, water and collision data on Windows and Apple Silicon Macs, including Live Sync and Undo/Redo.
- More reliable Live Sync through rapid edits, reconnects and playtests, including with several places open.
- Interrupted pushes preserve newer Studio edits.
- Studio opens behind your active app. Starting and stopping multiplayer tests keeps your keyboard focus.

### New tools

- Capture frame timing, memory and MicroProfiler data with `rbx perf`.
- Simulate lag and packet loss per client with `rbx net`.
- Read and edit supported protected properties with `rbx access` on Windows and macOS. Ask mode is the default.
- Compare complete RBXL/RBXLX files with another file or your project.
- Recordings include a frame overview; inspect individual frames with `rbx rf`.
- Check Luau syntax without running code using `rbx ck`.
- Add custom commands with `rbx plugin new`.

### Projects and editor

- Instance stores move to project-level `instances/`, keeping script folders clean. Existing projects migrate automatically.
- Optional `src/client`, `src/server` and `src/shared` mappings support different project layouts.
- Clearing Explorer search restores the tree instead of getting stuck on Loading.

## 0.3.4 - 2026-09-03

### Roblox packages

- Editing inside a linked package automatically marks it Changed and reports the package path, while keeping its PackageLink intact.
- `rbx pd` marks a package Changed, `rbx pp` publishes its changes, and `rbx pu` discards local changes and updates to the latest published version.
- Package actions work directly on Windows and macOS without selecting instances, opening dialogs, moving windows, or taking input focus.
- Package roots remain protected during sync, including duplicate names, nested packages, rapid edits, retries, and full or filtered updates.
- Discarding package changes restores the published contents instead of only clearing the Changed marker.
- Failed package edits restore the original properties and script source before returning the package to Up To Date.
- Renaming or repositioning a package root remains a normal local override instead of being mistaken for changed package contents.
- Removing a PackageLink is now consistently named **Unlink Package** and remains an explicit `rbx upl` action.

### Live Sync and Studio targeting

- Live Sync reports packages it marked Changed so publishing remains an explicit choice.
- Daemon and Studio restarts reconnect to the exact saved local file or published place, even when several Studio windows are open.
- Direct editor commands return compact results while preserving package-change warnings and actionable errors.
- Script verification accepts Studio's normal line-ending conversion without hiding real source differences.
- Rapid file replacements wait through brief Windows file locks and sync the final saved revision.
- Expected command failures stay in Renium's result and logs instead of filling Studio's Output.
- Studio status lists the available sessions when a project can't select one unambiguously.

## 0.3.3 - 2026-09-01

### Studio sync

- Rapid Studio edit bursts settle into one complete pull instead of saving an intermediate state.
- Live Sync status and pull acknowledgements stay attached to the Edit session while Play starts, runs, or disconnects.
- Entire package roots can be replaced or removed normally while direct `PackageLink` edits remain protected.
- Sync retries when Studio changes during an update, while conflicting project-file edits remain untouched.

### Playtests

- Play status follows the real session in the selected Studio window, including delayed starts and sessions whose server connects before the client.
- Stop waits for the actual client and server to finish without reporting a timeout after Studio has returned to Edit mode.
- Repeated start and stop commands finish or reuse the current session instead of creating overlapping tests.

### Place inspection

- Closed RBXL and RBXLX files can be searched directly with `rbx q` without opening Studio.
- `rbx cmp` compares every projected script with the current project and reports changed, missing, and extra scripts.
- Script comparisons ignore line-ending-only changes and duplicate sibling order.

### Agent workflow

- Live Sync status stays compact unless detailed diagnostics are needed.
- Renium updates run only from an update notice or an explicit request.
- PowerShell examples now cover Luau strings containing apostrophes.

## 0.3.2 - 2026-08-31

### Sync safety

- Reconciliation updates only content that actually changed, including after Studio or the daemon restarts; one edit no longer replaces unrelated place content.
- Independent Studio and project-file edits are combined, while true overlaps remain untouched until a side is chosen.
- Package links and package-owned trees stay protected during ordinary pulls, pushes, moves, renames, deletes, retries, and interrupted syncs.
- Duplicate-named instances, cross-service moves, new and deleted scripts, properties, attributes, tags, and references keep the correct identity in both directions.
- Full pushes finish without hanging or repeating the same work, and commands that require approval are rejected before Studio is contacted.
- A completed sync no longer returns as a false pending Studio change after Studio reconnects.
- Live Sync stays attached to its exact project and Studio place after restarts, and a second project cannot take ownership of the same place.
- Fast create, move, edit, and delete bursts no longer leave stale Studio tracking or false reconciliation differences.
- Cross-service object references resolve correctly when their target is unique and report a clear conflict when it is ambiguous.

### Performance

- Small Live Sync edits update only their affected instances instead of exporting an entire service or rebuilding the place.
- Clean daemon restarts resume from the saved common state without rereading unchanged Studio content.
- Full pulls, no-change pushes, and source-only pushes skip redundant decoding, transfer, and verification work.
- Large services reuse decoded project data and resolve script paths directly, keeping create, delete, and source edits responsive.

### Performance testing

- Studio performance profiles can constrain CPU, processor cores, memory headroom, and priority without changing FPS or taking input.
- Built-in device tiers provide quick degraded-performance tests and are offered only when the current computer can enforce them.
- Custom profiles can be saved, inspected, replaced, and removed with short `rbx pf` commands.
- Active profiles follow replacement Studio processes and survive daemon restarts; turning them off restores Studio's original processor and priority settings.
- Windows applies and verifies the limits directly. macOS and Linux report the feature as unavailable instead of claiming unsupported controls are active.

### macOS

- Renium now uses the installed Roblox Studio app directly instead of creating a separate Renium Studio copy.
- Studio updates are detected and the Renium helper is reapplied to the official app when needed.
- Pulling and exporting no longer opens the macOS Export Place save panel or repeatedly asks for file access.
- Background Studio automation stays behind the user's other apps and avoids activating, resizing, or taking over global keyboard and mouse input.
- Local-place reopening, recovery prompts, and package-change dialogs are handled without changing the selected project or losing the connected session.

### CLI and installation

- Studio, Live Sync, pull, and push commands consistently use the same connected runtime after reconnects.
- macOS setup and updates install a matching CLI, Studio plugin, helper, guides, and editor bundle.
- Command output remains compact and stable, including consistent spacing for scripts and filters that parse it.

## 0.3.1 - 2026-08-29

### Sync safety

- Small edits update only the affected Studio instances instead of rebuilding unrelated place content.
- Packages and their links remain intact during ordinary sync, reconciliation, moves, renames, deletes, and failed updates.
- References continue pointing to the same instances after cross-service moves and renames.
- Duplicate-named instances and reused internal IDs remain distinct instead of being merged, replaced, or duplicated.
- A failed or interrupted transaction leaves the previous Studio state intact and can be retried safely.

### Reconciliation and Live Sync

- Independent Studio and project-file edits are combined automatically; only overlapping changes require a choice.
- Creates, deletes, moves, renames, properties, attributes, tags, and script edits sync reliably in both directions.
- Pulls no longer turn unchanged place content into thousands of false Live Sync changes.
- Live Sync uses the selected project consistently and resumes cleanly after Studio or the daemon reconnects.
- Missing script files, `init` scripts, plugin scripts, and parent-sensitive instance classes keep their intended Studio structure.

### Performance

- Large-place pulls, cross-service moves, reference repair, and file publishing avoid reprocessing unchanged data.
- Bridge payloads are compressed and reused when safe, reducing repeated transfer and decode work.
- Small Live Sync edits no longer fall back to full-place reconstruction.

### Fidelity

- Round trips preserve package IDs, unknown properties, special Roblox value types, MeshPart sizing, Lighting settings, MaterialService settings, hierarchy, attributes, tags, scripts, and references.
- Editor property names map to the correct Studio properties instead of creating transport-only differences.

### Studio and editor reliability

- Background Studio automation no longer brings Studio to the front, resizes it, or takes over keyboard and mouse input.
- Every project command uses the same active Studio connection, avoiding false “no Studio connected” results.
- Custom and isolated daemon ports start and connect to the same intended session.
- Windows installs a native `rbx` launcher while keeping the command-file fallback available.
- Console output keeps stable spacing after labels so existing scripts and filters continue to match it.

## 0.3.0 - 2026-08-26

### Agent commands

- Commands return compact one-line results by default. Use `--output-mode pretty` only when expanded JSON is useful.
- Command help shows short runnable examples without daemon, bridge, or protocol options.
- Saved-data edits rely on Live Sync when it's active instead of repeating the same edit with a manual push.
- Update notices use the short `rbx upd` command.
- Luau can be piped through standard input, and sourcemap reads can use the existing cached map without rebuilding it.
- Explorer searches stay inside the selected service or subtree instead of returning unrelated project instances.

### Live Sync

- Reconcile starts without a direction choice when edits are independent. If both sides changed the same content, choose the version to keep once and Live Sync finishes starting automatically.
- A conflict choice applies only to that overlap; the saved reconciliation policy remains unchanged.
- CLI conflicts return both resolution commands directly; checking Studio's UI isn't required.
- Verify mode compares Studio and project files without changing either side or starting live writes.
- Existing `studio`, `editor`, and `none` startup settings migrate to the matching reconcile or verify behavior.
- Independent script edits from Studio and project files merge against their last common version. Overlapping edits stay pending, and CRLF/LF differences don't create conflicts.
- Studio source, hierarchy, duplicate-name, and sibling-order changes wake Live Sync without waiting for a full rescan.
- Settings changed in the Studio plugin while disconnected are delivered after reconnect; untouched plugin settings don't replace editor settings.

### Explorer

- Large searches show results quickly and keep loading ahead of fast scrolling instead of leaving blank gaps.
- Expanding and collapsing large trees responds faster without reloading work that's already available.
- Clearing a search no longer expands unrelated instances or moves the scroll position backward.
- Search help opens only when starting a search, and search navigation icons display consistently.

### Packages and sync safety

- Package contents can sync without replacing the existing `PackageLink` or losing the package relationship.
- `PackageLink` instances are read-only during normal add, copy, move, rename, property, import, and delete operations. Package desync remains an explicit command.
- Package changes are confirmed after a successful push on Windows and macOS without taking over keyboard or mouse input.
- A failed native import restores the original Studio tree; failed rollback data remains available instead of being discarded.
- Settings files changed during a prepared Studio update are detected before the update can overwrite newer edits.
- The Studio plugin no longer shows the extra editor-sync undo notification.

### Configuration

- Renium settings can be listed, read, and changed through the short `rbx cfg` command.
- The settings list includes current and valid values.

### Open Cloud

- Open Cloud can query analytics, manage game events and experiments, and configure personalized thumbnails.
- Multi-image thumbnail uploads and repeated array parameters are sent intact, and experiments can be started without request-body failures.

## 0.2.9 - 2026-08-22

### Agent commands

- Every automation operation is available as a direct top-level `rbx` command. Context IDs, the `rbx a` layer, command payload files, and manual daemon setup are no longer part of normal agent workflows.
- Short command names are the canonical agent interface, while descriptive names remain available for people using the CLI directly.
- Command help includes short examples for every operation and keeps descriptive aliases pointed at the same examples.
- Studio commands start or reuse the shared connection, select the project and place, and handle required workflow confirmation themselves.
- Image uploads with an explicit user or group owner can use Open Cloud directly without opening Studio.

### Updates

- Every agent command checks for a newer Renium version through one shared five-minute cache and prints a short update command when one is available.
- Failed update checks also respect the five-minute cache instead of retrying on every command.
- Generated agent instructions recommend keeping Renium current and rereading the instructions after an update.

### Open Cloud

- Native Open Cloud commands use Roblox's current request formats for assets, passes, badges, localization, servers, thumbnails, memory stores, Creator Store, advertising, experiments, events, and configuration.
- Pagination uses the correct token and limit fields for each Roblox endpoint.
- Creator Store search supports current page tokens, verified-creator filtering, price limits, and audio-duration filters.
- Multipart uploads and asset-version rollbacks send files and form fields in the formats Roblox expects.
- Public read operations can use `--anonymous`; authenticated permission failures remain visible instead of silently retrying under a different identity.

### Live Sync and agent commands

- Live Sync remains enabled when the shared daemon is replaced and resumes when the project is used again.
- Agents can rely on Live Sync after an edit instead of sending the same change with a separate push command.
- One-off file pushes accept paths directly, such as `rbx ps src/StarterGui/Menu.client.luau`.
- Successful push commands return only useful results instead of repeating paths, targets, direction, and selection details from the command.
- Project agent guides refresh when their contents change, including changes published under the same Renium version.

## 0.2.8 - 2026-08-22

### Live sync

- Studio edits wake live sync immediately instead of waiting for a polling interval.
- Files-to-Studio creates and deletions reach Studio sooner without scanning unrelated instances.
- Long Studio change waits leave the other bridge channel available, so Explorer edits and automation commands can run while live sync is idle.
- Pulls and pushes wait for the complete Studio connection and cancel stale change waits when needed instead of failing during a partial reconnect.
- Auto-connect keeps using its fast retry period after an unexpected disconnect.

### Explorer

- Creating, editing, or deleting an instance from the editor starts or reuses the Studio connection and reports an error if Studio did not apply the edit.
- Empty service roots accept their first child, and deleting the last child keeps the service ready for later edits.
- Multiple instances with the same default name remain separate and sync in creation order.
- Opening the add-instance menu no longer changes the selected node's expanded state.

### Project state

- Importing snapshots rebases the live watcher to the imported files instead of treating the import as new editor work.

## 0.2.7 - 2026-08-21

### Updates

- Updating the Studio plugin now asks how to handle each connected local place: leave it open, save and close it, or terminate it without saving. The choice can be remembered for later updates.
- Places closed for an update reopen at the same local file or published universe and place after the update succeeds or rolls back.
- Saving a local place before an update includes its live unsaved changes without opening Studio's save dialog or taking keyboard and mouse input.
- Direct Studio close commands require an explicit save or terminate choice instead of guessing what to do with local work.

### Studio automation

- Luau commands, waits, and movement return their results through Renium without adding automation messages to Studio's Output.
- Project instructions refresh when the installed Renium version changes, and agent commands stop once to request a reread before continuing with changed instructions.

## 0.2.6 - 2026-08-21

### Live sync

- Live sync now watches project files in the shared Renium daemon, so editor commands and direct `rbx` use the same pending work, retries, and Studio connection.
- File edits made while Studio or the editor is disconnected remain pending across daemon restarts and are sent when live sync starts against the matching place again.
- Nearby saves, directory changes, renames, deletions, new source roots, nested projects, and project configuration changes are collected into complete pushes instead of partial or repeated syncs.
- Transient connection failures retry with a bounded delay, while permanent failures keep the affected files pending until they are edited or explicitly retried.
- Starting live sync clears a pause left by an interrupted editor session, and stopping it also stops file watching when Studio's live-mode cleanup fails.
- Sync on save sends the saved files to Studio even when continuous live sync is off.
- Pulls, manual pushes, Git changes, package updates, link updates, history restores, and generated file writes coordinate with the watcher so their own writes aren't pushed back as new edits.
- Live-sync state files stay under `.renium` and are ignored by version control in new and existing projects.

### Script syncing

- Script batches that span multiple services are checked against Studio's current Script Editor content, and only scripts that didn't apply are retried.
- A script that still differs remains pending with its exact verification error instead of making the rest of the batch repeat or silently reporting success.
- Files edited during a Studio pull are kept as local pending changes instead of being consumed by the pull.

### Editor

- Git branch changes pause file mirroring and resume from the final worktree without stopping and rebuilding the whole live-sync session.
- Live-sync status shows daemon-side pending files and failures without running a second editor-side filesystem watcher.
- The obsolete **Run Import** setting is gone; pulling from Studio always updates the project files required by the operation.

## 0.2.5 - 2026-08-20

### Updates

- Agent commands report when a newer Renium version is available and include the command that installs it.

### Roblox Open Cloud

- Persistent and ordered data, memory stores, universes, places, messages, servers, restrictions, secrets, notifications, users, inventories, groups, social interactions, Team Create, assets, passes, Creator Store, localization, configs, analytics, ads, experiments, events, matchmaking, thumbnails, and speech generation have direct `rbx cloud` commands.
- Open Cloud work no longer requires endpoint paths or temporary JSON payload files for supported operations.
- API-key scopes and resource limits can be checked without exposing the key, including user-owned, dedicated group-automation, and resource-restricted keys.

## 0.2.4 - 2026-08-20

### Roblox Open Cloud

- Open Cloud commands work directly without Roblox Studio, a Renium connection, or a running daemon.
- Developer products can be listed, read, created, and updated from `rbx cloud product`.
- Any Open Cloud endpoint can be called with API-key or OAuth authentication, including JSON, form, file, and raw uploads and binary downloads.
- Newly saved Roblox credentials on Windows are available without restarting the editor.

### Renium Link

- Linked models can be renamed, moved, and rotated without losing their link.
- Updating a linked model preserves its local name, position, and orientation while refreshing its contents.

### Installation

- Windows updates keep `renium` and `rbx` pointed at one current installation instead of leaving stale executable copies behind.

## 0.2.3 - 2026-08-20

### Package editing

- Editing scripts, properties, attributes, or children inside a Roblox package no longer removes its `PackageLink`.
- Renium accepts Studio's package-edit warning without taking over keyboard or mouse input on Windows and macOS.

### Studio plugin

- The Studio toolbar shows the Renium icon again.

## 0.2.2 - 2026-08-20

### Agent guidance

- Agents now start with a compact Renium guide and open only the task-specific instructions they need, reducing context use without removing commands or safety rules.
- New and existing projects receive the same topic guides whenever Renium creates or refreshes their instructions.

## 0.2.1 - 2026-08-19

### Syncing

- Files-to-Studio pushes now stage the affected roots before editing and restore them if any batch or final verification fails, including package-backed trees.
- Package edits desync only the package relationships on changed paths; unrelated packages, services, and descendants remain untouched.
- Large editor transactions are streamed through the bridge in bounded chunks instead of failing at the WebSocket request limit.
- Cross-service moves use one project transaction and preserve scripts, references, sibling order, and destination package links.
- Script writes are verified against Studio's active script document, standalone deleted script files remove their instances, and deleted `init` scripts keep their children as folders.
- Filtered native imports still apply independent source and property changes outside the imported services.
- Property commands resolve Roblox aliases and case differences to the property name actually stored in project data.

### Studio automation

- GUI presses verify that the target is visible, unobstructed, and receives the expected Roblox button events before reporting success.
- Input coordinates follow the real rendered viewport, including scaled and simulated views, while the input shield yields only to Renium's own pointer events.
- Device simulation state is kept by the shared daemon and restored when a new Studio process connects; stopping simulation also closes Studio's remaining emulator toolbar on Windows and macOS.
- Play and Studio status report active device simulation so automation doesn't silently test with mobile controls.
- Ambiguous selectors and other already-reported command failures exit once without printing the same error twice.
- Installed launchers keep pointing at the selected Renium version after an update.

## 0.2.0 - 2026-08-18

### Syncing

- Added direct `rbx pull`, which starts or reuses the shared daemon, waits for the matching Studio place, and keeps temporary data under `.renium`.
- Pulling a place to files and pushing it back preserves classes, properties, attributes, references, scripts, hierarchy, and sibling order.
- Fixed full pushes swapping instances that share the same parent.
- External script `Source` reads and writes now use the exact source file instead of exposing `__SOURCE_EXTERNAL__` or creating a shadow property.
- Snapshot export and import reproduce every generated project file byte-for-byte.
- Model and store inspection use stable instance references that remain useful across commands.
- Removing the final imported tree from a service also removes its empty settings store and directory.
- File-backed commands and sourcemaps work from experience roots and individual place folders.
- Multi-place commands infer the current place when possible and list valid places when selection is ambiguous.
- Pull and push results are smaller and clearly identify their direction and affected services.

### Studio and automation

- Runtime listing no longer repeats the same Studio runtime under separate collections.
- Unpublished Play and local-server runtimes retain the name of their originating Edit place.
- Studio Auto-Recovery dialogs are dismissed automatically for Studio instances controlled by Renium.
- Minimized Studio windows can be restored without taking focus or moving ahead of other windows.
- Input commands recover from a minimized `1x1` Play viewport before interacting with it.
- Held-key results include the effective duration, and navigation results include the arrival radius and final distance.
- MP4 recording can be controlled with `record-start` and `record-end` without JSON, context IDs, or recording IDs.
- Device status reflects Studio's actual emulator selection, including devices selected outside Renium.
- `device stop` resets Studio to the normal desktop device, while disconnecting Renium no longer changes the selected device.
- Device changes fail clearly during Play instead of reporting an edit-side state that does not affect the running client.
- Fixed portrait resolution, native-versus-effective density, rounded viewport dimensions, duplicate device IDs, and detailed native-orientation output.
- Local-server tests no longer print repeated missing-plugin-icon errors.

### Direct agent commands

- Added offline `project-validate`, `script-search`, `script-grep`, and ranged `script-read` commands that need no Studio connection, daemon, context, or JSON payload.
- Limited script searches report complete totals, deterministic ordering, and whether results were truncated.
- Creator Store search and Roblox documentation reads work directly without context binding.
- Creator Store results are compact by default and identify the requested asset type.
- Public Creator Store models use Roblox's plugin-accessible loader when ownership-only loading APIs reject them.
- AI model generation, creator-job polling, and local image validation have direct commands.
- Roblox documentation results contain readable text and signatures instead of minified page markup.
- Created, cloned, renamed, and moved instances return their resulting stable identity for immediate reuse.
- Batched `prop:Source` reads return exact external script text, and requested field filters no longer leak unrequested internal paths and IDs.
- Property and source values beginning with hyphens are accepted normally.
- Repeating an edit that changes nothing does not rewrite files or report false changes.
- Misspelled properties are rejected, while explicit property scope remains available for hidden or newly introduced Roblox properties.
- Typed instance references survive cloning and are remapped to cloned targets.

### Projects and configuration

- Single-place projects can be converted safely into multi-place experience layouts.
- Place addition validates IDs, aliases, destinations, and game identity before moving project files.
- Failed place conversions and renames restore the original project instead of leaving a partial migration.
- Place add, rename, and reorder results state when the project must be rebound.
- Project initialization previews every file and directory it will create or update and rejects wrong-type collisions before changing anything.
- Validation accepts absent source roots for mount-only projects but rejects existing source roots that are not directories.
- Rojo imports preserve JSONC comments, no longer mistake URLs for comments, and omit redundant default, empty, and null fields.
- Generated adapter modules remain visible through `find`, `tree`, `inspect`, and `bb` without invalidating later validation, builds, path explanations, or syncback.
- Adapter syncback counts only source files changed by the user.
- Writable mounts update their backing files, read-only mounts reject edits, and mount-only projects export through their projected content.
- `explain-path` identifies transformed sources, excluded rules, matching selectors, sync direction, and property or attribute decisions.
- Configuration paths are normalized across Windows, macOS, and Linux, and removing the final scoped value cleans up its empty file and directory.
- File and directory imports identify each file as `create`, `overwrite`, or `unchanged`; unchanged files are not rewritten or included in later push work.
- Newly imported scripts can be inspected before their service has a `.renium` settings store.
- `doctor` reports complete parser errors, normalized paths, correct repair instructions, and deterministic diagnostic bundles.

### Packages and version control

- Link application can initialize a missing service store without requiring a Studio pull first.
- Links can be removed permanently while keeping their instances as editable project content, and empty manifests and lock files are removed automatically.
- Link results distinguish total and active targets, return stable root IDs, and report no changes for unchanged refreshes.
- Fixed link path forms, ordinals, hierarchy counts, exact source reads, and manifest and lock cleanup.
- Wally directory `init.lua` files retain their children, lock versions are correct, unchanged normalized package trees are detected, and forced refresh is supported.
- Detaching a reusable package writes embedded scripts back as exact editable source files.
- Package operations return stable IDs for the complete materialized subtree, and repacking includes local packages referenced by the project.
- Unchanged packages, stores, and formatted project files retain their bytes and timestamps.
- Git initialization adds missing Renium rules without replacing existing user rules.
- Renium policy files and JSONC configuration retain LF line endings on Windows.
- Binary-store merges support independent field edits and clearly reject conflicting edits to the same field.
- Merge-driver paths containing spaces work correctly.

### Editor and installation

- Completed editor updates close their progress notification before asking for a reload.
- **Check for Updates** appears at the bottom of the main Renium menu.
- Windows installs directly from the selected platform ZIP instead of downloading the CLI again.
- Windows and macOS verify the Studio plugin against the signed release manifest before installation.
- macOS and Linux show editor choices and **Exit** before downloading release files.
- Normal command output no longer includes bridge startup lines, channel-ready messages, internal build timings, or per-service import progress.
- Generated `RENIUM.md` uses direct commands and avoids unnecessary daemon checks, help calls, recursive searches, temporary JSON files, repeated reads, and local-server tests.
- The Windows `rbx.cmd` fallback correctly handles complex inline Luau containing `cmd.exe`-sensitive syntax.

## 0.1.9 - 2026-08-17

- Preserved non-Archivable instances and current script documents during full Studio pulls without cloning the live DataModel.
- Kept package roots and cross-service references intact during full pushes while avoiding unnecessary package snapshot work.
- Reduced full-push time with direct project builds, filtered native exports, and faster retained-package matching.
- Verified a 95,691-instance pull-to-push round trip with no class, property, attribute, source, or reference differences.
- Sent only plugin settings edited while disconnected when the matching editor reconnects.
- Started automatic connections immediately in every Studio state and made the plugin show connection progress without moving its controls.
- Kept bound automation contexts valid when editor and direct `rbx` commands share one daemon.
- Improved source projection, duplicate-name mapping, sourcemap generation, model pivot restoration, and native export mutation checks.

## 0.1.8 - 2026-08-15

- Added one Windows installer that selects x64 or ARM64 automatically and recovers when the wrong platform ZIP was downloaded.
- Fixed standalone installers to verify downloads through the release update manifest instead of a removed checksum file.
- Kept the editor extension active without a workspace and initialized project features when a folder becomes available.
- Removed the redundant SPDX, checksum-list, and XML plugin files from GitHub release assets.
- Reorganized the editor menu around sync, project, and tool groups with shorter result-focused descriptions.
- Prevented full pushes from exposing an empty place while replacing Studio's service trees.
- Replaced untyped daemon command forwarding with a versioned operation registry, bound project/place contexts, stable errors, and review receipts.
- Reused one Studio daemon across editor windows and direct `rbx` commands, and waited for the matching Studio runtime before runtime-bound operations.
- Fixed pulls when Roblox omits engine-generated descendants while cloning and serializing a Studio tree.
- Made signed updates component-aware so the CLI, extension, and Studio plugin stay on compatible versions during installation and rollback.
- Added read-only `.rbxm` and `.rbxmx` inspection through `rbx view` and stdin support for batched project reads.
- Shortened the generated `RENIUM.md`, removed temporary command-payload files, and clarified ordinary Play, local-server, multiplayer, and Luau runtime selection.
- Split the daemon and automation implementation into focused modules while removing duplicated parsing and dispatch code.
- Removed the missing-property-database warning from installed builds that use the plugin's bundled schema.

## 0.1.7 - 2026-08-15

- Changed exact-window recordings from animated WebP images to H.264 MP4 clips on every supported platform.
- Kept normal pulls on Studio's plugin serializer and ignored Roblox-reserved service attributes that third-party plugins cannot recreate.
- Kept automatic plugin reconnection active across Edit, Play server, and Play client states with bounded retry delays.
- Fixed repeated Play start and stop cycles, multi-client runtime selection, stale runtime pins, and false duplicate-launch errors.
- Added a targeted cross-platform input shield with an orange viewport outline and the current Renium version while preserving system window switching.
- Added shielded plugin virtual input for Linux and retained exact-window native input on Windows and macOS.
- Fixed typed automation requests for plural instance properties and attributes, settings-ID editor mutations, live status, bound project validation, and Revert history.
- Returned useful Revert results, normalized Windows script paths, and bounded documentation snippets from minified Roblox pages.
- Prevented Play clients from creating edit-session locks and fixed console filtering results.
- Kept editor update prompts open until the user chooses **Update**, **Later**, or closes the prompt.

## 0.1.6 - 2026-08-14

- Restored a minimal `renium.project.jsonc` marker in every place root and create missing place markers during binding and place setup.
- Included every serialized instance in sourcemaps while retaining source-file paths for scripts.
- Fixed cached update checks when GitHub returns `304 Not Modified` and added **Check for Updates** to the editor menu.
- Moved the generated agent guide to `RENIUM.md` and kept project-owned instructions in `AGENTS.md`.
- Added one Unicode-marked guide instruction to `AGENTS.md` and to `CLAUDE.md` when Claude's file doesn't already refer to `AGENTS.md`.
- Fixed generated GitHub release notes so the change list appears directly below **What's Changed**.

## 0.1.5 - 2026-08-14

- Moved Renium's agent instructions into one packaged Markdown file instead of embedding the full guide in Rust and JavaScript source.
- Kept generated project instructions named `AGENTS.md` and the Claude pointer named `CLAUDE.md`.
- Included the agent guide in platform archives, editor extensions, installers, repairs, and signed updates.
- Updated generated instructions to use the installed `renium` and `rbx` commands from `PATH`, with stable fallback paths for shells opened before installation.
- Removed the unrelated snapshot-importer tagline from the CLI help header.

## 0.1.4 - 2026-08-13

- Added normal platform ZIP installers with short installation instructions and direct launchers.
- Added editor selection to the Windows and macOS installers, with an explicit exit option.
- Made the Windows launcher install directly from an open ZIP and put `renium` on the user PATH.
- Centralized signed update checks in Rust with a shared five-minute cache and ETag revalidation.
- Made every new Studio runtime receive the cached update result without repeating GitHub requests.
- Removed direct plugin downloads from the editor extension and kept matching CLI, extension, and plugin versions together.

## 0.1.3 - 2026-08-13

- Added batched Roblox Open Cloud requests with bound universe and place expansion, escaped path parameters, conditional requests, retry metadata, and Data Store support.
- Added Creator Store and user-inventory search, supported asset insertion, model generation jobs, local image validation, image upload, and official Roblox documentation reads.
- Added script search, saved-source reads, literal grep, exact multi-edit operations, temporary camera capture, and adjustable pathfinding speed to the agent API.
- Added ordered keyboard and mouse sequences sent only to the selected Studio window without moving the system cursor or taking focus.
- Added exact-window Studio and play-client recording as animated WebP clips.
- Reorganized the Rust crate into feature-focused modules and updated release validation for the new layout.
- Removed generated Studio plugin bundles from source control; release and extension builds now create them from the Luau source.
- Added startup update notifications and one-click signed updates for the installed editor extension and Studio plugin.

## 0.1.2 - 2026-07-10

- Added experience projects with separate place roots, exact GameId/PlaceId routing, active-place switching, and renameable aliases.
- Bundled the matching Renium CLI and `rbx` launcher with the extension so projects no longer need local executable copies.
- Pinned every multi-channel command to one Studio runtime so two windows on the same place cannot be mixed during chunked or parallel sync.
- Added Studio undo recordings, Explorer selection preservation, CollectionService tag tracking, and first-error batch stopping for filesystem-to-Studio changes.
- Hardened imports and uploads with recoverable stale-file backups, bounded/expiring sessions, explicit cancellation, and fail-closed place-guard reloads.
- Ignored line-ending-only script differences while preserving the filesystem's CRLF/LF convention.
- Made ordinary compile, test, package, and release builds use checked-in generated metadata without launching Studio; API and icon refreshes are now explicit maintenance commands.
- Expanded release verification with Rust formatting/Clippy, Linux/macOS tests, generated plugin parsing, binary round-trip fixtures, and Rojo builds for both plugin formats.

- Added the **Renium Store Viewer**: a new tab in the Renium sidebar where you drag any `.renium` file (or double-click one in the Explorer) to see its full instance tree, class icons, properties, attributes, and script source. Decoding goes through the `renium view` CLI so what you see matches exactly what syncs.
- Added **renium-link**: control one script's source from a single place and mirror it into multiple targets. Sources can be local, git (public/private), or Wally. Targets are read-only mirrors (with an `L` Explorer badge and native read-only editor) until broken. New commands: `Apply Links`, `Add Link`, `Link Status`, `Break Link`, `Reveal Link Source`, plus a `renium-link.json` manifest watcher and `renium.link.*` settings.
- Hardened **Wally** sync: dropped the Rojo dependency (packages are imported directly), added `wally.lock`-aware no-op detection, and added multi-realm import (`shared`/`server`/`dev`) via `renium.wallySync.realms`.
- Added a locked, version-checked release build that regenerates the VSIX and both Studio plugin artifacts with a checksum/provenance manifest.

## 0.1.1

- Added Renium GitHub sync in the main Renium panel with a normal tab layout, src-only default scope, git status/fetch/pull/commit/push flows, and optional Studio re-apply after pull.
- Added explicit `Renium: Export Game File` support for writing the current `src` tree to `.rbxl` / `.rbxlx` place files without changing GitHub sync's src-only default.
- Added git parsing/redaction helpers plus focused tests for GitHub sync parsing and commit-message utilities.
- Reduced Studio live-sync polling pressure by using a 250ms base interval with adaptive idle/error backoff.
- Wait for the Properties webview to signal readiness before sending the first property payload.
- Updated live-sync documentation and setting descriptions to match current two-way sync behavior.

## 0.1.0

- Initial VS Code/Cursor extension scaffold for Renium with native executables.
- Added command palette actions, status bar entry, output logging, live sync process management, and auto-sync-on-save support.
