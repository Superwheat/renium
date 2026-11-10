# Changelog

## Unreleased

### Removed

- `rbx bcl` (`bytecode-clone-instance`) is removed: `rbx cp SERVICE -i SOURCE -I PARENT` makes the same copy, with `-n NAME` and `--push`.
- `rbx clk` (`click`) is removed: `rbx inp -p N click X,Y` clicks a viewport point, `right X,Y` uses the right button and `down X,Y wait MS up X,Y` holds it.
- `rbx ky` (`key`) is removed: `rbx inp -p N key KEY` presses a key and `rbx inp -p N hold KEY MS` holds it.

### Improvements

- Scripts read by function: `rbx sr FILE --outline` lists a script's functions as `FIRST-LAST name(params)` lines, `rbx sr FILE --fn NAME` prints only the named functions, and `rbx sg TEXT --fn` prints each function that holds a match once instead of single lines, so understanding one function no longer means printing its whole script.
- `rbx mcp` serves Renium to agents as one MCP tool, `rbx`, whose input is the argument list (or `steps`, several lists run in order until one fails, plus optional `stdin` and `cwd`), so a call needs no shell, quoting or Bash-or-PowerShell choice and returns the command's own output. `rbx mcp --install` registers it with the installed agents (Claude Code, Codex, Cursor, Windsurf, Gemini CLI, OpenCode), keeping a backup and every other setting of each config and skipping one that already has a `renium` entry; `--uninstall` removes it. `rbx setup` registers it too, and the agents' Renium note says the tool takes the same arguments as a list.
- A command on a service that has no store yet (`rbx bs Lighting`, `rbx tr Teams`, `rbx in`, a batch op) pulls that service from Studio first, when a daemon is running, instead of stopping at "pull from Studio".
- `rbx ba --batch` carries every kind of store edit: besides adds, an op may be `set` (`path`, `props`, `attrs`, `tags`), `clone` (`path`, `name`, optional `parent`), `rename` (`path`, `name`), `move` (`path`, `parent`) or `remove` (`path`), and an add or set takes a script's text from `sourceFile`, syntax-checked as `rbx ck` does. Ops apply in order and may name instances earlier ops created by `#ref`, values behave as in `rbx bs`, every touched store and script file is written together, and a failing op names its index and writes nothing. The result lists one receipt per op under `ops` (formerly `created`); with `--push`, renames, moves and removals reach the open place first, each touched store is pushed once, and the result reports one `verified`.
- One call reads several saved values: `rbx bg SERVICE TARGET A B C` prints one object, `rbx in SERVICE T1 T2` one array, and `rbx q FILE -n A -n B -c Class -s TEXT` matches any of the repeated selectors in one decode of the place file.
- `rbx play -s` waits up to 60 s until the session's players exist and reports `playersReady`; `--no-ready` returns at once. `rbx play -r` stops, lets Live Sync apply the file edits it deferred during Play (`appliedPending`), then restarts.
- `rbx inp ... --result LUAU` evaluates an expression on the same client after the actions and `--shot PATH` screenshots it, so acting and checking is one call.
- `rbx cs --wait SECONDS` blocks until a Studio is connected instead of needing a shell loop.
- `rbx ba` and `rbx bs` take `--push`, which sends the edited instance to the open Studio place right after the file write, scoped to that instance and without a review; `rbx bs` sets several properties in one call with repeated `-p NAME=JSON`.
- `rbx play -s --console N` returns the last N server console entries with the start result, after `--until` when given; `--stop-after` then stops the session, so one call runs a play check.
- `rbx play -s --at PATH` stands the first player's character on an instance (or at a position `x,y,z`) once it spawns, before `--until` runs, so a touch check needs no hand-written move.
- `rbx play -s --console N` also returns the client's console as `clientConsole` in ordinary Play, so a client-side check needs no second session.
- Every `rbx` command refreshes `RENIUM/map.md`, a compact map of the project's stores (each service's instances two levels deep, sibling runs folded, the services without a store), and the agent guide points agents at it instead of an opening `rbx tr`.
- A Play-targeted call (`rbx lc`, `-p N`, `--server`) fails at once when no Play session is running or the index is above the session's client count, instead of waiting out the bridge wait twice (16 s).
- `rbx play -x` returns as soon as the test has ended instead of also waiting out the controller's 2 s restart settle; the next start still waits for the settle.
- Stopping a Play session asks the server and client runtimes to stop at the same time with a short wait, instead of one after the other with 1.1 s each; a stop used to take a constant 2 s before Studio's own teardown was even checked.
- `rbx play --until` returns a table value as JSON instead of `table: 0x...`; the compact start result's `clients` counts players, not bridges; and a client selector above the session's client count fails at once instead of after the connection wait.
- `Position` and `Orientation` fold into the CFrame of any instance that has one (parts, attachments, cameras), and a CFrame value takes `"orientation": [rx, ry, rz]` in degrees next to its position. Setting an attachment's Position used to be refused, and a rotation needed the nine matrix numbers.
- `rbx play --result` works with `--until-console`, and `--client-result 'LUAU'` evaluates once on the first client after either wait holds (`clientResult`), so a play check that needs state after a console line is one call instead of a second session.
- `rbx cp -n NAME --push` names the copy and pushes it in the same call, with `studio` and `verified` in the result; a copy used to need `cp`, then `rn`, then a push.
- A BrickColor property takes a palette name such as `"Bright red"` in `rbx ba` and `rbx bs`, stored in the canonical numbered form.
- A reference property (PrimaryPart, Attachment0, SoundGroup, Part0, an ObjectValue's Value) takes an instance path such as `"SoundService.SFX"` in `rbx ba` and `rbx bs`; the store keeps it as a path reference and the push resolves it by name. Agents used to need the full Ref object with store ids and sibling ordinals.
- `rbx bs Lighting -p ClockTime=18.25` stores the value as `TimeOfDay` (`18:15:00`), the property Studio saves; a stored `ClockTime` used to make every push of the Lighting store fail with "did not retain". The agent guide shows service-level edits without a target and the one-time pull of a service that has no store yet.
- `rbx play -s --until-console TEXT` returns as soon as a server or client console line contains the text, instead of waiting a fixed game time; the result's `untilConsole` names the side, the seconds and the matching entry, or `timedOut`.
- A push read-back reports `Tags` through `GetTags`, so `rbx bs -p 'Tags=[...]' --push` verifies like any property, and the agent guide sets tags with `bs` instead of a Luau edit.
- A push no longer fails with "Roblox did not retain Color" on a part whose stored color is not an exact byte value: the engine keeps part colors as bytes and truncates (0.3 becomes 76/255), so the plugin's retention check now allows one byte per channel.
- The agent guide says a `--where` edit lists every match it touched, so agents act directly instead of listing first.
- `rbx rn --push` and `rbx bs -p Name=... --push` rename the instance in the open place before the push pairs it. A renamed instance that had never been pushed used to be created again under its new name while the old one stayed; a Lobby of 32 parts had 40 after eight renames.
- `rbx rn SERVICE -i SCOPE --where QUERY --replace FROM TO` renames every instance under the scope that matches the query and has FROM in its name, as one store edit and one push, with `renamed`, `studio` and `verified` in the result.
- `rbx br SERVICE TARGET [TARGET...]` removes several named instances in one call, and `rbx br --push` reports `verified` when Studio destroyed every one of them.
- `rbx ba` and `rbx bs` refuse a property that a push skips (hidden or engine-managed in Studio's API, such as a TextLabel's `Font`) and name the replacement, instead of storing a value Studio never receives.
- `rbx ba`, `rbx bs`, `rbx mv` and `rbx rn` with `--push` report `verified`: `true` when the live read-back in `studio` holds every pushed value at the instance's path and class, `false` when Studio kept a different value or has no such instance, and nothing when a value's live shape cannot be compared.
- A Studio opened for a named daemon is passed to that daemon's bridge ports only while its launch ticket is under fifteen minutes old and a daemon still listens there; stale tickets are swept at the next launch. A Studio whose process id matched an old ticket used to be sent to ports nobody listened on and never connected.
- `rbx play --stop-after` and `rbx play -r` stop a session started with `-p N`; the stop request used to be refused because it still named the players.
- `rbx br --push` destroys the removed instances in the open place as one undo step, and `rbx br SERVICE [TARGET] --where QUERY` removes every instance under the target that matches a bulk-read query, up to 500 per call.
- A `--push` on `ba`, `bs`, `mv` or `rn` sends the pushed scripts' files with the store, so a new script created under a folder or under StarterPlayerScripts arrives in Studio with its text instead of an empty Source.
- `rbx ps` with up to 25 changed scripts reads their sources back after the push, so `verified: true` answers without a separate read; `--verify-sources` still forces it for larger pushes.
- Property values take compact shapes by the property's type: `[scale,offset]` for a UDim, `[xScale,xOffset,yScale,yOffset]` for a UDim2, `[min,max]` for a NumberRange, `[r,g,b]` bytes for a Color3 when a channel is above 1, and a plain item name such as `"Neon"` for an enum, stored in the canonical form.
- `rbx bs SERVICE [TARGET] --where QUERY -p NAME=JSON` sets properties on every instance under the target that matches a bulk-read query (`is:Class`, `tag:Name`, `Prop>0.5`, `or`), up to 500 per call; `--push` pushes the matches together and reads them back.
- A play start's `--console` entries fold Studio's stack-trace framing into a `where` field on the error it belongs to and collapse consecutive repeats, and the compact result reports `untilSeconds`, how long `--until` took to hold.
- `rbx ba` and `rbx bs` accept `Position` on a part and store it as the part's CFrame (keeping an existing rotation), instead of writing a property Studio never applies.
- A new instance pushed with its properties counts as one change toward the review threshold, in the daemon and in the Studio plugin, so a single new part no longer opens a review.
- The root agent guide carries recipes for the common edits, so a task that adds or changes instances, writes a script or runs a play check needs no further guide.
- `rbx init` writes a CLAUDE.md that imports RENIUM.md (`@RENIUM.md`), so Claude Code has the guide in context before its first command instead of reading it each session; an existing CLAUDE.md with the read-me pointer is migrated on the next command, and the editor extension writes the same pointer.
- `--push` on `rbx ba` and `rbx bs` verifies pushed script sources, so its `verified` field covers a script adopted from a written file.
- A `--push` result carries `studio`: the live read-back of the edited instance (path, class, the properties and attributes just set), so an add, set, move or rename needs no separate read to confirm it.
- `rbx mv` and `rbx rn` take `--push` too, and `rbx ba --batch --push` pushes every created instance once per store. A pushed move reparents the same instance in the open place (reported as `reparented`) before the scoped push, instead of leaving a copy under the old parent.
- A CFrame value may be given as `{"_type":"CFrame","position":[x,y,z]}` with an optional 3x3 `orientation`, not only as twelve components.
- The root agent guide carries recipes for moving and renaming, for editing existing scripts, for many instances at once and for the JSON value shapes, and routes the guides in a compact list.
- `rbx ps`, `rbx prop` and `rbx del` above the review threshold no longer wait 90 s for a Studio review panel nobody answers: they apply nothing and return `heldForReview` with the change list (`review`: path, class and changed names per instance), so the caller reads it and reruns with `--yes`. Pushes from the editor extension and Live Sync keep the Studio panel.
- A bare instance name resolves to the instance nearest the service when it is the only one of that name at that depth, so `rbx ba ReplicatedStorage -I Misc` and `rbx tr StarterGui Main` pick the top-level folder and screen even when a frame deep inside shares the name; several at the same depth stay ambiguous.
- `rbx inp` takes `--result`, `--shot` and `-p` after the actions as well as before them.
- `rbx ps` with script files named on the command line applies them at once; the large-push hold (`heldForReview`) stays for whole-project pushes and store files.
- `rbx wait 30s` (also `500ms`, `2m`) waits that long and nothing else, for a sampler's window, instead of a condition that never holds.
- `rbx l` and `rbx lc` grow the default 10 s run limit to cover the `task.wait(N)` sleeps written into the code, and to 30 s for code that waits inside a loop, reporting `timeoutSeconds`; `-t` still sets it.
- `rbx ps -s SERVICE` pushes a service's store file.
- `rbx tr` prints an indented tree in text mode: one `Name Class` line per instance, one space of indent per level, ` +N` for children not shown, `Name[2]` for a later sibling of the same name and `Name[1-4]` for a run of alike siblings; a name that holds a bracket is quoted. `rbx f` prints `path Class` lines, a shared parent once as `path:`, and says when the limit cut the list. Properties, attributes and tags asked for with `-F` (`rbx tr ... -F tree,Size,Visible`, `rbx f ... -F lookup,Anchored`, `rbx in ... -F props`) print in the same text: `Name=value`, `@Attribute=value` and `#tag` after each line, or one per line under `path Class` for `rbx in`. Values are short: numbers to three decimals, `x,y,z`, a UDim2 as `{xs,xo},{ys,yo}`, a color as `#rrggbb`, a CFrame as `x,y,z;rx,ry,rz` (position, then Orientation in degrees), an enum by its item name, a reference as its path and a binary value as its type and size; alike siblings fold only when these fields match too. `--output-mode json`, an explicit `-o`, or a `-F` that names other record fields (ids, indexes, source paths) returns the records as before.
- `rbx sg` and `rbx ss` print each script's path once and then its hits as `LINE: text` without the indentation, a line over 240 characters cut short, and a last line when the limit left hits out.
- A held push prints at most 20 review rows in text mode, one short line each: `+ PATH Class` for an added instance, `- PATH Class` for a removed one and `PATH: Property, Property` for changed values; `moreReviewRows` counts the rest.
- Store edits that report `changedPaths` (`ba`, `bs`, `cp`, `mv`, `rn`, `br` and the rest) leave out of their text result what it repeats: the store file `changedPaths` already names, the store index and the split path (one dotted `path` instead), the push summary's own `ok` and `persistence`, and the read-back's path and class once `verified` is true.
- `rbx l` and `rbx lc` print the returned values as text, one per line (a string as it is, or quoted when it would read as another value; `nil`; anything else as compact JSON; `ok` when nothing was returned), after any captured `print`/`warn` lines, and a failure prints `error: MESSAGE` with its traceback; `--output-mode json`, `--detach` and `--collect` keep their JSON.
- `rbx play` prints the server and client consoles a result carries as `[level] message` lines after the result line, collapsing identical messages into ` (xN)` and messages that differ only in an id of six or more digits into ` (+N similar)`; `rbx co` prints its entries the same way, 50 unless `-n` says otherwise.
- `rbx wait` prints its seconds with one decimal and leaves out the echoed condition, also when it times out; `rbx inp` leaves `action`, `ok: true`, a `virtual` `inputMethod` and the echoed screenshot path (`shot: true`) out of its text result; `rbx co` leaves `ok` and `folded` out of its header line.
- `rbx status` leaves out the top-level `teamCreate`, a zero `placeVersion` and a zero Live Sync `pending` in text mode, and `rbx status --all` prints the same listing as `rbx cs`.
- `rbx status` and `rbx cs` list the runtimes of one Studio without their runtime and launch ids in text mode, and `status` prints the native verdict counts only; a play start leaves out `mode: play` and a `serverReady` that `playersReady` implies, a stop its teardown wait, and `rbx me` on an existing script answers with the edit count.
- `rbx sg -E` reads the query as a regular expression and `rbx sg --in PATH` searches one folder or file under the project; `sg` prints 50 lines unless `--limit` says otherwise, `rbx ss` prints the script paths, and `rbx sr` prints `FIRST-LAST/TOTAL` before the source instead of repeating the path.
- `rbx sg -c` prints each script's path and its number of matching lines instead of the lines.
- A plain `rbx l` or `rbx lc` run prints at most 100,000 characters in text mode, followed by a line that says how many there were.

### Bug fixes

- `rbx --daemon NAME so FILE` (or `RENIUM_DAEMON_NAME`) no longer starts the default daemon. The Studio's plugin still connects to the default ports first; when no daemon listens there, the named daemon answers on them until that Studio has moved over, turns every other Studio away, and gives the ports to a default daemon that starts meanwhile. A running default daemon passes the window on as before.
- A Content property the reflection database does not know yet (`Folder.ExplorerIconContent` in Studio 0.747) keeps its type through a pull. It was stored as a string, and the next push of that instance failed in Studio. Pull a store written before this fix again.
- `rbx bs --push` pushes the edited instance alone. It used to push the instance's whole subtree again, so one attribute set on a large folder or model took many seconds and could make Studio ask whether to stop the plugin.
- A store-file push the review held (`rbx ps instances/SERVICE.renium` with more than the threshold's changes) returns the hold and its `--yes` hint again; it used to go on to read Studio back and report "did not retain pushed project changes" for instances nothing had created yet.
- `rbx dev set` no longer reports a 1.5 s timeout for a device Studio did switch to: when the setters outlive the plugin's reply window, the status read afterwards stands in for the result and carries `settledAfterTimeout`.
- A push that creates a text object with `TextScaled` on no longer fails its read-back on `TextWrapped`, which Studio keeps on with scaling; the push had applied nothing and reported "did not retain native supplemental properties". A store written with `TextWrapped` off next to `TextScaled` on now carries the value Studio keeps, so the next push of that store no longer rewrites the label and trips the transaction's change check.
- Live Sync no longer reports a conflict on a part color nobody changed: colors Studio keeps in bytes (a part's `Color`) compare by their byte values, so 0.3 in the files and 77/255 in the open place are one color.
- A build from source reports the commit it was built from in `rbx dr`, bug reports and the daemon log; the hash stayed on an earlier commit until one of a few watched files changed.
- The extension no longer damages non-ASCII names and text: it decoded each chunk of a command's output on its own, so a multi-byte character split between two chunks turned into replacement characters in the Explorer, in command results and in git output.
- Extension packages built by the Release and Verify workflows carry the class icons committed to the repository; packaging replaced them with the icons of the Studio installed on the build machine.
- `tools/build-release.ps1` accepts macOS binaries; its Mach-O check compared an unsigned magic number with a negative literal and refused every one.
- `rbx bs` with several `-p NAME=JSON` (and `--where`) could set later properties on a different instance or leave a half-applied edit when one assignment failed; it now applies every assignment to the instances selected at the start and writes nothing unless all of them succeed.
- Setting an enum property to a number that is no item of the enum (`-p Material=99999`), or to an item of another enum, was stored and built into the place; both are now refused like an unknown item name.
- `rbx bss`, `rbx br`, `rbx ba`, `rbx bcl` and batch add, clone, remove and move ops could change a read-only link package and leave it drifted; they are now refused like `rbx bs`, and `--override-packages` allows them on purpose.
- `rbx fmt` put a stray space before a leading comment and before the value after a block comment, and `fmt --check` accepted that; both now use the plain indent.
- A `renium.project.jsonc` saved with a UTF-8 byte order mark, as Windows PowerShell's `Set-Content -Encoding utf8` writes it, failed in the CLI with "Invalid JSONC ... expected value at line 1 column 1" and in the extension; both read it now, and `rbx fmt` removes the mark.
- `rbx pi` and `rbx rev --sync` left `.renium-trash-...` copies of the project beside it; the command now finishes deleting them before it exits.
- `rbx ir` and `rbx init` refused the project `rojo init` creates ("Invalid property 'Ambient' ..."); service properties without `$className` are now converted.
- `rbx build` turned a folder with `init.luau` into a plain Folder (dropping its source) or failed with "already exists as ModuleScript, expected Folder" when the folder was a tree node with attributes or properties, a mounted directory, a nested project's source root, or held a model, `.txt` or `.json` file; it is now built as the script, and an explicit `$className` still decides.
- `rbx build` ignored `Position` and `Orientation` set in a project tree node or `.meta.json` and left the part at the origin; both now place the part.
- `rbx cmp PLACE --full` reported changes on a place just built from the project for properties Roblox renamed (SoundId/AudioContent, BrickColor/Color, Image/ImageContent); they now compare equal.
- A merge with a preference (`rbx vcm --prefer ours|theirs`, and the same merge when a sync resolves toward one side) silently dropped an instance and everything under it when the other side had removed its parent. The instance now follows the preferred side: the parent comes back when that side still has it, and otherwise the instance stays where that side has it.
- Luau code and text that start with `-` or `--`, such as a comment, `--!strict` or `-5`, are taken as values by `l`, `lc`, `wait`, `play`, `inp`, `sg`, `ty`, `co --grep`, `q`, `prop -j` and `report -m`, and as the first keyword of `ss` and the first OLD/NEW pair of `me`, instead of being rejected as unknown options; later `ss` keywords and `me` pairs still need `--` before such a value.
- `rbx dm list`, `dm clean` and `dm stop` no longer treat a leftover daemon record as running when its process ID now belongs to another program, and `dm stop --force` removes the record instead of ending that program.
- `rbx dm stop` no longer prints taskkill's "SUCCESS" line, so its JSON output is valid.
- `RENIUM_DAEMON_NAME` takes precedence over a stored `daemon` setting, as `--daemon` already did.
- `rbx l -f FILE` runs a file saved with a UTF-8 byte order mark instead of rejecting it.
- A named daemon that has run for more than seven days is still found; commands used to start a second daemon beside it.
- `rbx mcp` answers a JSON-RPC batch with one array, as protocol 2025-03-26 requires.
- `rbx mcp` exits when its client closes stdin even while a command runs, and on Windows a command no longer outlives a killed server.
- When the daemon already has 16 open connections, the next command is told so and asked to retry, instead of failing with a socket error.
- `rbx -r DIR COMMAND` no longer creates DIR for commands that work in an existing project; it reports that the folder does not exist, and only `rbx -r DIR init` creates it.
- A merge no longer keeps a reference to an instance the other side removed, which left a store that failed to build into a place. Without a preference `rbx vcm` reports it as a conflict; with one the instance comes back when the preferred side still has it, and the reference is cleared otherwise.
- `rbx prop` and `rbx del` above the review threshold say so: the result carries `heldForReview`, the change list in `review` and the `--yes` next step, as `rbx ps` does. They used to print only `{"ok":true,"changed":false}`.
- Store edits (`rbx bs`, `rbx ba`, batch props) check bool, number and string values against the property's type: a number given to a string property is stored as text, a numeric string given to a number property as the number, and anything else is refused with an example instead of failing later in `rbx bep` or a push.
- Setting or adding an attribute whose name Roblox refuses (empty, over 100 characters, starting with `RBX`, or with characters other than letters, digits, `_`, `.`, `-`, `/`) is refused.
- Removing, renaming or moving an instance that holds a read-only link package (`rbx br`, `rbx bs -p Name` or `-p Parent`, batch ops, `rbx rm`, `rbx rn`, `rbx mv`) went through and left the link drifted; it is refused unless `--override-packages` is given.
- `rbx rn --where` could rename instances inside a read-only link package; a call with such a match is refused as a whole unless `--override-packages` is given.
- A reference property written as a path into another service (`-p 'Value="ReplicatedStorage.Shared.Util"'`) stays on its target: it follows renames and moves (`rbx rn`, `rbx mv`, `rbx bs -p Name`, batch ops), is cleared when the target is removed, and `rbx bep` no longer fails on it or points it at a same-numbered instance of another service.
- A reference path that names no instance in the stores is refused, `Pad[2]` picks the second of two same-named instances, and a build that cannot resolve a reference names the instance, the property and the path.
- A Live Sync merge that keeps or brings back a script one side removed (for a reference or a child the preferred side added, or for its changed properties) also brings back its source file from the side that still has the script.
- The extension's property panel refuses renaming or moving an instance that holds a read-only link, as `rbx rn` and `rbx mv` do, unless packages are overridden, and stores a value in the form of its property, as `rbx bs` does (a numeric string becomes a number, an enum name its item); a value the property cannot take is refused instead of written to a store that fails the place build.
- A reference into another service whose id also exists in another store could be treated as unchanged by Live Sync, renumbered by `rbx vcm`, or pointed at the local copy by a link or Wally update; each now reads the id as one of the path's service.
- `rbx bep -s SERVICE` pointed a reference into a service it left out at a same-numbered instance of an exported service; it now refuses and names the instance, the property and the path.
- A reference whose stored path is out of date is found by its id: the place build and a push reach the renamed instance, a push sends its current path, and a pull of one service updates the paths other stores hold.
- In Live Sync, a reference in one service to an instance the other side removed from another service is a conflict; a preference restores the instance or clears the reference.
- `rbx ba` accepts a reference to the instance being added, and one `rbx ba --batch` accepts a reference to an instance a later op adds; a path that names nothing says to create the target first or add both in one batch.
- On macOS and Linux the reservation of an update check could outlive the check while another thread was starting a child process, so the next check skipped its refresh; the reservation is now released when the check ends.
- Aligning a store's ids in a pull, merge, first pairing, model JSON adapter or service projection no longer rewrites the id of a reference into another service that shares it, and a reference retargeted from an instance of its own store to one of another service with the same id is seen as a change.
- A merge no longer lets a reference bind to an instance the other side put at its removed target's path: it is a conflict without a preference, and restored or cleared with one; when a merge restores an instance for a reference, it also restores what that instance refers to, with script sources.
- A pull keeps an instance's store id on the instance Studio reports it for after a rename, instead of giving it to a new instance that took the old name.
- A pull keeps the store id of every instance a push created, after Studio renames or moves it: the import now carries the ids of a pushed subtree's descendants as well as its roots, and Studio reports those instances under them. A new instance that takes the old name gets its own id, so a reference from another service still names the renamed instance. An instance Studio moved into another service is no longer reported under its old store's id.
- The native Windows paths work on Studio 0.747 again. The binary reader's wrapper, the attribute signal's new argument order and the ObjectValue relay are found by their structure, so a push imports natively and keeps its native verification instead of falling back to the plugin transport; Studio 0.746 still matches.
- macOS: native snapshots and captures work on Studio 0.746 again; the local-save serializer is found by its function boundaries, callers and ownership instead of an exact prologue and allocation size. `rbx native-probe` now reports Studio actions as native, accepts the `RobloxStudio.bin` layout for viewport capture and names a missing Screen Recording permission without opening a prompt, and says which operations macOS has no native implementation for (service import, attribute observation, package actions, PID-scoped input).

## 0.5.9 - 2026-11-06

### New

- On macOS, Studio's native queue serves live Terrain observation and saved-field edits without reopening the place. Saved-field writes require a fresh matching read before reporting success, and pulls refresh `Lighting.Technology` from the native getter.

### Improvements

- On macOS, native operations give a busy Studio queue the operation's remaining time instead of a fixed short budget.
- Installing or removing a macOS plugin in an explicit folder leaves Studio installations alone. Setup recognizes complete Renium patches whose rollback backup belongs to another installation.
- Errors for invalid mesh geometry values identify the instance and property that need correction.

### Bug fixes

- The macOS and Linux installer no longer stops with "The inherited Renium lifecycle lock owner is no longer running" on the first nine days of a month: the process start time it hands to the CLI is compared with the spacing `ps` adds to single-digit days collapsed on both sides.
- On macOS, cold pushes no longer wait ten seconds for a busy Studio UI to prepare native access. Preparation uses the UI run loop and falls back to the plugin within one second if it cannot finish, with the reason shown in `rbx status`.
- On macOS, push, pull and Live Sync no longer wait for an empty native task before checking saved fields or capturing identities. Identity captures also use plugin identities if Studio closes its response at the deadline, with the reason shown in `rbx status`.
- On macOS, native reads keep the complete response when Studio closes the connection after sending it, avoiding "Invalid argument (os error 22)" during sync and saved-field edits.
- Live Sync, push and pull can use plugin identities when Studio has nil or reused UniqueIds. On macOS, identity reads also fall back when their time budget expires. `rbx status` shows the reason.
- `rbx so` and `rbx ro` open an Edit window when only orphaned test server or client windows have the place open. On macOS they close those orphaned windows first (`closedOrphans` in the result), because Studio there opens a new window beside a running test server as a client of that test, and the launch no longer mistakes an orphan for the Studio it just started.
- `rbx pi` recognizes a place's format from its contents, including XML saved with an `.rbxl` extension. Imports and stores preserve binary engine data and animation rig tables, including the lowercase `name` and `parent` tables, so rebuilt places keep them.
- `rbx pi` keeps supported saved service settings such as `Lighting.PrioritizeLightingQuality`, and stores `Lighting.TimeOfDay` within one day, as Studio does.
- Live Sync's initial pairing and pushes recognize Studio's loading rewrites of lighting times, animation rig tables, emote and accessory ordering, font caches and reconstructed CFrames. Push verification also accepts equivalent asset URLs for both modern Content values and legacy asset properties.
- Initial pairing keeps Studio's migration from `AccessoryWeld` to `AccessoryRigidConstraint`. Duplicate accessory names no longer cause the wrong saved weld to be removed.
- Pushes leave omitted, default and engine-managed fields alone instead of attempting unsupported resets or reporting false retention failures. Optional fields that a capture does return must still match the requested value.
- Initial pairing recognizes older collision hull formats and ignores reissued texture-pack IDs and recounted union triangles consistently when comparing individual fields or whole instances.
- `rbx cmp` avoids differences caused by class defaults, equivalent asset URLs, generated rig tables and service state a project cannot carry. Authored text, attributes and rig tags still report real changes.
- On Windows, saving retries a brief file-sharing lock instead of immediately failing with "Access is denied".

## 0.5.8 - 2026-11-02

### Bug fixes

- On macOS, reading a Studio window's title gives up after one second when that Studio is still starting: `rbx status` no longer waits several seconds per loading window.

- `rbx audio` commands issued right behind one another wait up to three seconds for the previous command's lock instead of failing with "Another audio command is in progress".
- A failed play-target Luau run waits up to half a second for the engine's error lines, so `consoleErrors` carries a failed module's own message on a loaded Studio too.

- `rbx pr PATH -p N` and `rbx ty TEXT --path PATH -p N` right after a client joins wait up to eight seconds for the GUI path to exist instead of failing with "matched nothing": the client builds its PlayerGui a moment after it connects.
- A reviewed saved-field edit that reopens the place (`rbx prop` on a field with no plugin API) waits up to 90 s for the reopened Studio to connect instead of 20 s; a loaded machine needed longer.

- A Studio that opened the Roblox sign-in dialog instead of its place is reported as signed out: `rbx so`, `status` and the Studio commands stop waiting for it at once with that verdict instead of waiting out the connection timeout.
- `rbx status` reads only the startup part of each Studio log when it diagnoses an unconnected Studio; a long session's log of tens of megabytes made the daemon answer status in seconds.
- A project's own Studio is recognized on macOS when its document path resolves through the data volume (`/System/Volumes/Data/Users/...`): the project root is compared in the same canonical form.

- A command that starts a daemon waits up to 15 s for it to answer instead of 5 s: on a loaded machine the daemon came up after the command had already given up with "did not become ready".
- `rbx go` waits up to ten seconds for the character's Humanoid when the character has just spawned, instead of failing at once with "no humanoid".

- A daemon no longer inherits the place pin of the command that started it: the pull after a `prop`, `del` or `cr` under Live Sync filtered Studios by that stale place and skipped the project's own Studio.

- A Studio that another daemon launched keeps its play server and the clients of a multiplayer test on that daemon's bridge ports: the Edit window records the ports it is on when a test starts, and the play runtimes connect to them instead of the saved ports.
- A leading `-r NEW` after `--daemon NAME` still creates the folder and reports that it holds no project, instead of failing on an unexpected argument.
- `rbx pl` answers with the `historyId` of the sync history entry it records, so `rbx rev --sync ID` can undo that pull by id instead of guessing `latest`.
- Recordings are tracked per Studio window: two Studios can record at once, `rbx re` without an id ends the only active one and names the active ids when there are several, and one caller's `re` no longer ends another Studio's recording.
- A command run from a project whose place is open in several Studio windows of the same name binds the window whose document lives in that project instead of refusing as ambiguous.
- Same-named Studio windows whose documents live in other Renium projects no longer make a project's commands ambiguous: `rbx pa`, `rbx pn` and `rbx po` run without a Studio of their own while those windows stay open.
- `rbx l --server`, `rbx lc` and the other play-target commands of one project no longer reach another project's play server or clients while that project tests: a play request keeps its own Edit window bound on its thread, so `l --server` without a running test of its own refuses with "No play server is connected".
- A failed Luau run on a play server or client keeps its `consoleErrors` in the JSON answer; the runtime label in front of the message no longer drops them.
- `rbx perf analyze` finds Roblox's MicroProfiler parser by name under the libmp `latest` release and accepts its October 2026 build; the previous fixed asset link answered 404.
- `rbx play -x` during a server or client `rbx wait` stops the test in normal time and the wait fails at once with "This Play runtime is stopping or restarting"; the wait polled its condition in one run as long as its timeout, which kept the stop from reaching that runtime for up to 40 s.
- `rbx pr PATH --world` lets the pointer rest on the part before it clicks, so a `ClickDetector` counts the press right after `rbx go` instead of only once the camera has settled.
- `rbx rev --path STORE --print` prints a saved store in its text form instead of its compressed bytes.
- Studio windows Renium opens start at least eight seconds apart, counted across every Renium daemon on the machine: two windows starting within the same seconds both refreshed the Roblox session, one with the cookie the other had just replaced, and Studio then signed every window out.
- A scoped push that deletes a script open in the Studio script editor closes that document; before, the commit failed with "Could not restore open script document" and rolled the push back.
- The macOS input shield that `rbx ky`, `inp` and the other input commands start gets five seconds to come up instead of one; a loaded Mac answered "startup timed out".
- A Studio opened through a named daemon (`rbx --daemon NAME so FILE`) reaches that daemon: the launch is noted by process, and the default daemon, where the plugin connects first, sends the window on to the named daemon's bridge ports for the session.
- The plugin's change tracker no longer freezes Studio when thousands of same-named siblings are removed or inserted at once (a destroyed 6000-part folder made Studio show "Renium.rbxm is not responding"): sibling ordinals are rebuilt at most once per frame instead of once per change.
- `rbx dm stop` on macOS and Linux no longer reports a killed daemon as still running while it waits to be reaped.
- `rbx l`, `co`, `sc`, `ui` and `wait` in the Edit window queue on their own Studio's gate: a long Luau call or wait in one open Studio no longer makes the same commands on another open Studio answer "Studio is busy" after 5 s. Requests to one Studio still run one after another, pushes, pulls and the other Edit operations still run one at a time across Studios, and the busy answer names the request holding that Studio.
- `rbx ps` and a Live Sync push read `liveSync.changesThreshold` from the project they push: the private copy a push stages its files in carries no place configuration, so a threshold set with `rbx cfg set liveSync.changesThreshold 0` was lost there and the plugin's own setting decided the review.
- A review of more than 1 MiB of changes, which reaches Studio in chunks, applies the project's `liveSync.changesThreshold` like a smaller one; before, the chunked upload dropped the threshold and the plugin's own setting decided whether the push was held.
- `rbx prop` on a saved field with no plugin API (`Workspace.StreamingMinRadius`, `Lighting.Technology`) opens the Studio review when the change count is above the threshold, and `rbx rv skip` answers `skippedByReview: true` and leaves the field; `--yes` writes it at once. Before, the native write skipped the review.
- `rbx prop --yes -o ORDS` on a saved field of one of several same-named siblings reaches that sibling: the ordinals are handed to the native property writer. Before, the write was refused as ambiguous and asked for the ordinals the command already carried.
- A reviewed `rbx prop`, `del` or `ps` asks for its hold notice on each daemon request itself, so the operation's stored parameters carry no transport flag and a reviewed `rbx del` is never parsed with an unknown `--notices` option.
- A play request submitted before its client or server connects waits for that runtime and then runs behind the runtime's own gate, the same gate every later request to it takes, so two requests to one client never run at the same time; a bare client request with several clients takes the gate of the client it runs on. A project whose Studio is not connected never queues a play request on another project's runtime.
- A push, pull or Live Sync comparison whose link manifest names a read-only package that is missing or cannot be read stops with the package error instead of comparing as if nothing were packed; before, the packed scripts' files and stripped PackageLinks read as differences, and a store build could demote a packed script to a Folder.
- `rbx pl` and a Live Sync pull keep the store's values for the viewport camera when the store was written before `Workspace.CurrentCamera` was saved: the camera Studio's reference names is matched to its saved row by identity, never by name, and the pulled store records the reference.
- Two `rbx --daemon NAME` daemons started at the same moment take different port blocks, and the one that lost a block waits for its own daemon's discovery record instead of taking the winner's answer on that port for its own: before, the loser's command talked to the other daemon and timed out while `dm status NAME` found no daemon.
- A push, pull or comparison in a project with many read-only link packages decodes each package once per operation instead of once per capture and per store.
- An additive store push (`rbx ps STORE -u`) sends the store's values for every instance it lists, so an edit to an imported or pulled instance reaches Studio instead of only matching it by path.
- `rbx dm stop` waits up to five seconds for a daemon that is finishing a Studio request.
- `rbx plugin process PID --terminate-studio` waits for the process to exit and answers with its final state.
- The MicroProfiler controls accept Studio's newer control protocol minor version instead of refusing every capture.
- `rbx ad syncback --preview` prints its result on one line in JSON output mode like every other command; before, it was always pretty-printed.
- A renamed directory of scripts under Live Sync (`ren src\ReplicatedStorage\Shared Shared2`) no longer hands the deleted scripts' identities to the created ones: fresh identities are numbered from the store's size, so the row a push removed earlier in the same collection was reused by the next addition, and Studio resolved the new folder to the script it had just deleted. The identities a collection retires stay out of reach of its additions.
- The first comparison of a consumer place (`rbx lon` after `lkp --share`) no longer pulls Studio's PackageLink into the store: a link at the root of a read-only link whose package holds no PackageLink there is dropped from Studio's capture before the merge, as the baselined comparisons already treated it, so the store stays without it until `upl` removes it from Studio.
- `rbx pl` and a Live Sync pull keep the store's values for the viewport camera (the Camera the Workspace root's `CurrentCamera` names): the comparisons hide that camera and `rbx bs` refuses to edit it, so a Studio edit of its `FieldOfView` no longer reaches `rbx bg Workspace Camera FieldOfView`.
- A request to a play server or client no longer holds up the Edit window or another Studio: the gate a play request queues on is resolved among the runtimes its own Edit window launched, so a `wait --server` or a long `lc` beside requests of other Studios no longer falls back to the Edit gate (`l --edit` answered "Studio is busy" after 5 s, and `play -x` waited for the wait's whole timeout). A server or client selector that resolves to nothing yet queues on its own gate instead of the Edit gate.
- `rbx wait` in the Edit window runs as two-second slices and lets queued requests through between them, so `rbx l --edit` can set what the wait is waiting for; before, the wait held the Edit gate for its whole timeout and every other command answered busy.
- `rbx ky --hold-ms` over 30 s and `rbx inp hold KEY MS` or `inp wait MS` over 10 s hold for the time asked (up to 60 s): long waits are split into the plugin's ten-second waits and sent to the client as consecutive calls, with the key held across them. Before, `ky` failed on the bridge's 30 s response timeout and `inp` cut each action at 10 s.
- `rbx go -t` over about 115 s walks for its whole timeout (up to 300 s) as slices the character resumes from, instead of dying with Studio's 120 s runner limit.
- `rbx wait -t` and `play -s --until-timeout` above 1800 s are answered with `timedOut` at their own timeout, up to the hour the guide promises; before, the command gave up at the daemon transport's 30-minute limit without it.
- `rbx play --kill-orphans` with `RENIUM_PLACE` (or `--place`) set to a place path closes the orphans of that file: an orphan knows its place by file name only, and the path's file name now matches it.
- `rbx l -f FILE`, `rbx rs -o FILE` and a default-named `rbx rs` recording resolve against the directory the command runs in, like `sc -o` and `perf --out`; before, `l -f` and `rs` resolved against the project root and a default recording landed in the project.
- A Luau run that fails because a module did not load answers `consoleErrors` with the lines Studio logged, as runtime.md says, beside the `error` text that still carries them; the JSON error of a failed command now carries the daemon's structured detail fields.
- `rbx re` ends a recording at once: it makes no Studio call and no longer queues behind an `l --edit` or push holding the Edit gate, so a recording stopped during one is not longer than asked.
- `rbx inp -p 1 click Workspace.Props.Crate[1]` on its own clicks the part: a world path takes the viewport-point route whether or not another action precedes it; before, a lone world click was sent as a GUI click and failed with "matched no GuiObject".
- `rbx perf micro-start` names what Studio's MicroProfiler control probe answered (size and words) when the protocol is not the one Renium knows, instead of only "protocol changed".
- `rbx perf heap` summarizes `unparentedInstanceReferences` from the Instances Studio's version 2 reference report lists under `Refs.Roots`, where the leaked Part now appears; before, the summary read the empty legacy list. The result always carries `target`.
- `rbx ty TEXT --path BOX` waits a frame after the box takes focus before sending the text, checks that the text arrived and re-focuses and sends once more when it did not; the result carries `textObserved`. Before, text sent in the frame the focus landed was dropped now and then while `chars` and `enter` reported success.
- `rbx prop` and `rbx del` under Live Sync now reach the files: after Studio applies the edit the daemon pulls the touched service into its store and advances the shared baseline, and the answer carries `filesPulled: true` (`filesError` names a pull that failed). Before, the edit changed Studio only, `rbx lst` reported itself settled with `pulls: 0`, and `rbx bg` kept the old value until the next full comparison.
- `liveSync.changesThreshold` is a real setting: `rbx cfg list` shows it, and a push or direct edit with more changes than the project's threshold opens the Studio review (0 reviews every change) whatever the plugin's own setting says. `rbx prop` without `--yes` can open that review again; before, the CLI's approval flag disabled the review outright, so the threshold was never consulted and `--yes` changed nothing.
- `rbx prop --yes` on a saved field with no plugin API (`Lighting.Technology`, `Workspace.StreamingMinRadius`) writes it through the native property writer and answers `savedFieldsApplied: 1`; when that writer is unavailable the error names the property and the way to set it. Before, the plugin reported a protected write it could not apply and the command answered `changed: false, ok: true`.
- `rbx prop` and `rbx del` while Studio is in Play are refused up front ("Studio is in Play ... stop the session (`rbx play -x`)"); before, the edit was held for a review that Play hides and skipped unanswered after 90 s.
- `rbx prop` and `rbx del` answer with the fields a push prints: `persistence`, `skippedByReview` when the review declined the edit, `savedFieldsApplied`, `protectedApplied`, `unsupportedProperties` and `packageModified`. `rbx del` takes `--yes` and `--no-review`, and the global `rbx --yes` pre-approves both commands; before, `rbx --yes del` was still held for the review. A `del` above the review threshold is held for the Studio review exactly as a push is (`rbx rv apply|skip` decides); before, the daemon never treated a delete as reviewable.
- A `prop`, `del` or `ps` held for the Studio review prints the hold notice on the caller's own stderr ("held for review in Studio (id=..., changes=...); `rbx rv apply` or `rbx rv skip` decides ..."); before, the daemon logged it and the caller saw nothing while it waited.
- `rbx prop -i ID -p '[]'` resolves the settings id again; before, the daemon read the resolved path as text (`Workspace.Props.Sign`) and failed with "invalid type: string, expected a sequence".
- A json adapter builds valid Luau: object keys are written as quoted strings (`return {["k"] = 1}`); before, a long-bracket key right after `[` opened a long string (`return {[[[k]]] = 1}`) and `rbx ck` and `require` both failed.
- Replacing a built adapter in renium.project.jsonc no longer breaks every later `rbx ad build` with "The filename, directory name, or volume label syntax is incorrect (os error 123)": the stale baseline entry's target is read back in the form the baseline writes.
- A two-way `txt` adapter whose StringValue has not been pulled into its store yet no longer fails `rbx ps` of that store and `rbx ad syncback` with "Projection target 'ReplicatedStorage.Note' does not exist"; the file stays the source until a pull records Studio's copy, and a `from-project` adapter whose target is missing says so and names `rbx pl`.
- `rbx pa` moves the `instances/` stores into the new place root with the project files; before, they stayed at the experience root and `rbx f`, `rbx tr` and the pushes of the converted place saw an empty project.
- `rbx pa` on an unpublished place gives the same answer whether or not the daemon had seen the project's Studio: the current place keeps an entry of its own (place id 0, the start place), the named place is added beside it, and `--game-id` is recorded. Before, a never-opened project became the named place and an opened one dropped the game id.
- `rbx so FILE` records the opened place in `.renium/studio-target.json` like `rbx ro` and `rbx sx` do, so an unpinned command selects that window when several Studios are open, and `rbx ro` on a place that is already open names its `runtimeId`.
- `rbx --daemon NAME` starts a daemon of its own on the first free control port from 8790 up (bridge ports beside it) instead of fighting the default daemon for 8780, and `rbx dm status` / `rbx dm stop` without a name address the `--daemon NAME` in effect; before, the auto-start failed with "did not become ready within 5s on control port 8780" and `--daemon other dm status` answered for the default daemon.
- `RENIUM_PLACE` beats a `place` stored with `rbx cfg set place ... --scope user`, as `--place` does; before, the stored pin won. The undocumented `PLACE` environment variable no longer pins anything.
- `rbx status -w N` keeps waiting for a Studio that is still connecting (right after `rbx so --no-wait`, or after `rbx dm stop` restarted the daemon) and binds it when it connects; before, it answered "no connected Studio matches this project" while the next `rbx status` a moment later listed the client.
- `rbx report -m MESSAGE` masks secrets in the GitHub issue link too; before, `token=...` was masked in report.md and report.json but copied unmasked into `issueUrl`.
- `rbx dr --bundle out.zip` writes the zip it promises; any other path still gets a folder. Before, the extension was stripped and `out/` was written.
- `rbx setup --repair` says what it does: it reinstalls the Studio plugin and the agent hints and leaves PATH as the installer set it; the help had promised PATH entries it never wrote.
- Deleting a store file (`instances/Lighting.renium`) under Live Sync no longer empties that service in Studio. The push of a store the baseline holds and the files lack is refused, and `rbx lst` names the file and the way back: `rbx pl` writes it from Studio, a restored file is pushed again with `rbx ps instances/Lighting.renium -u`. A `rbx lon` that finds such a store missing records a conflict instead of a deletion (`--prefer studio` rewrites the store from Studio). Before, every instance of the service was removed from Studio, and only a store restored from version control plus a push brought them back.
- `rbx lon --prefer editor` after a delete-vs-edit conflict (a script edited in the files and destroyed in Studio) no longer fails with "Studio did not retain the reconciled project state: src\ReplicatedStorage\Shared": the merge keeps the folder a kept script lives in even when Studio's capture, having lost that folder's last script, carries no entry for it.
- Live Sync's first comparison no longer reports `property Source: absent vs "..."` for a script `rbx lkp` packed into its store: Studio's capture is compared with the packed text folded into its store row, so a project with a packed link starts Live Sync without a preference. A Studio edit of the packed text still shows as a difference.
- A push that queued changes Studio did not apply answers `verified: false`. Before, `rbx ps instances/Workspace.renium -u --yes` from a never-pulled `rbx pi` project answered `ok: true, verified: true` while changing nothing in Studio.
- `rbx bs` refuses to set properties on the Workspace viewport camera (`Workspace.Camera`), which Studio owns and Renium never syncs; before, the edit was stored and every push reported it as unchanged.
- Commands run from a folder inside the project find the project above it again (`cd src; rbx ps ServerScriptService/Main.server.luau`, `rbx lks`, `rbx f`, `rbx bg`). Before, the upward search started from a relative `.`, ran out of path at once and answered "No renium.project.jsonc was found from the current directory upward".
- A case-only rename of a script file (`Main.server.luau` to `main.server.luau`) maps to the existing script instead of creating a second one: file stems are matched the way the projection names them, without regard to letter case.
- A new `DIR/init.luau` and its sibling files push in one `rbx ps DIR` and in one Live Sync cycle. Before, the sibling was collected first and created the directory's instance as a Folder, and the init file then failed with "Could not identify the previous instance at ...". Init files are now collected before their siblings.
- A store that cannot be decoded blocks its push with an error that names the file (`Could not read the store instances\Lighting.renium: Invalid settings bytecode magic`); before, `rbx lst` showed only "Invalid settings bytecode magic".
- `rbx rev --sync ID` with an unknown id answers "No sync history entry ID" and points at `latest`; before, it printed an operating-system path error.
- macOS upsert pushes now preserve Studio-only instances and imported metadata by using plugin writes or a saved-place reopen when the native structural locator is unavailable.
- Live Sync and project pulls on macOS now use the plugin's serialized identities when native identity capture is unavailable.
- A push after Studio reopened a place older than the files no longer fails with "Studio did not retain pushed project changes" for scripts whose editor tabs Studio restored: the plugin verifies a written script against its Source property when the restored tab still shows the older text. Before, the push had landed but was reported as failed, and `ps --verify` disagreed with itself for a moment.
- Recorded reconcile conflicts no longer block a scoped push or the status that follows it with "Reconciliation state has unresolved changes" (the message itself recommended that push): a push of a conflicting path resolves it toward the files and drops it from the list, and the remaining conflicts wait for the next `rbx lon`.
- `rbx ba` and `rbx ba --batch` refuse a class Studio cannot instantiate (`Player`, `Terrain`, a service) instead of storing it; before, the push of that store failed for good, and an imported `Player` left the Studio window unusable until Studio was closed.
- `rbx me src/ServerScriptService/Main.server.luau OLD NEW` (the guide's form) edits the Studio script the file maps to and writes the same replacement into the file; before, a file path was taken for a new script and refused with "className must be Script, LocalScript, or ModuleScript".
- `rbx del` no longer needs `-c CLASS`: the instance is matched by its path; before, the plugin answered "Editor instance entry className must be a non-empty string".
- A script whose source is stored inline in its store (what `rbx lkp` leaves behind after packing a subtree) is no longer treated as deleted by the projection: `rbx sm`, `rbx bep`, `rbx in` and a push keep it. Before, pushing the packed service deleted the script in Studio.
- `rbx plugin info` prints the plugin directory as a plain path.
- A full push (`rbx ps --yes`) that both recreates an instance from the store and rewrites a script Studio had edited now writes the script too; before, the planner's targeted instances narrowed the push to those instances, the script was skipped and the push failed with "Studio did not retain pushed project changes" after applying the instance half. Live Sync's reset after a Studio-side edit hit the same path.
- A Live Sync push that Studio answered with protected writes it cannot apply through the plugin (on macOS, a large batch replaces the service through the plugin import and reports the service root's streaming settings) now compares those writes with the saved place before giving up: writes whose values are already current no longer roll the push back, and the error names the properties that remain (`Studio could not apply 5 protected write(s) (Workspace.StreamingTargetRadius, ...)`). Before, a 6,000-instance batch under Live Sync on macOS was rolled back for five unchanged Workspace settings and stayed pending.
- Saved-data commands take a positional target after `-f STORE` or `-s SERVICE` (`rbx bg -f STORE Lobby.Door Anchored`, `rbx bs -s Workspace Lobby.Door -p Anchored --bool true`); before, the target landed in the service slot and the command asked for a property, refused "not both" or dropped the target.
- `rbx bs -p anchored` matches the property name case-insensitively like `rbx bg` does, and `rbx ba -f STORE` / `rbx bim -f STORE` on a store that does not exist yet seed it with its service root instead of writing a root-less file that broke every later `rbx bep`.
- `rbx rn` and `rbx bs -p Name` refuse to rename a service root, and a renamed `Terrain` keeps its protection; `rbx mep -i Lobby.Door` accepts a dotted target like the other commands.
- `rbx bep -s A,B` builds only the named services; `rbx bb search` prints `visibleIds` in a stable order and `rbx bb --project-root` reports the real `instances/` store path; a `globIgnorePaths` pattern such as `src/generated/**` also hides the folder itself from the projection, sourcemap and builds; `rbx bep` prints the import base as a plain path; `rbx mv --to-service X` into a service without a store names the missing store; `rbx ip --path-json -r DIR` prints project-relative paths.
- `rbx ps` without `--yes` opens the Studio review again when a push exceeds the plugin's change threshold, as the guide and `ps --help` say; the review had been skipped for every daemon push since the automation API arrived. Live Sync and startup reconciles keep applying without a review.
- `rbx rv apply` and `rbx rv skip` answer a held push at once: the decision no longer queues behind the push that is waiting for it (before, `rv` timed out after 30 s and the review expired after 90 s).
- A push skipped at the Studio review fails with "The push was skipped at the Studio review ... nothing was applied" and names `rbx rv apply` and `--yes`; before, the message was "Reconciled changes require review before Studio can be updated".
- `rbx dp` discards only the pending file edits, as documented: Studio edits made before the discard are still pulled afterwards. Before, the discard also cleared the plugin's record of Studio changes, which then stayed unsynced until the next full comparison.
- Studio changes to a service whose store push is blocked are no longer acknowledged by the pull that set them aside: the plugin keeps them pending and Live Sync looks again every five seconds, so they land once the blocked edit is pushed again or discarded with `rbx dp` (which now records the discarded files as the shared baseline). Before, they stayed unsynced until the next full comparison.
- Live Sync pushes a file that `rbx rev --sync latest` restored (and any file a directory rename brought into the project): the watcher now walks a directory it sees created, renamed or removed instead of trusting one event for the directory itself. Before, the restore landed in the files, status showed nothing pending, and Studio kept the pulled content until an explicit `rbx ps`.
- A script tab open in the Studio editor no longer makes every command fail with "no connected Studio matches this project": the window title then reads `<place> - <Script> - Roblox Studio`, and the place path and the `--place` name are read from it in that form too.
- `rbx collab` merges concurrent edits of different lines of one file when one side saves it with CRLF line endings (as many Windows tools do) or saves an older copy moments after the other side's edit arrived; a save that only switches LF/CRLF keeps the room's line endings, and lines both sides changed keep both versions between `<<<<<<< NAME` and `>>>>>>> room` markers. Before, the host's edit was silently lost and both folders kept only the guest's version.
- `rbx collab` keeps a save that restores exactly the text another participant's edit had just replaced: the restore stands as the user's choice and reaches the room. Before, a save made within five seconds of the remote write was read as an unchanged older copy, and the remote edit was written back over it.
- `rbx collab` marks lines both sides changed even when both saves were made before either reached the other: each save records the shared text it was folded into, and the side whose lines the room put first writes both versions between `<<<<<<< NAME` and `>>>>>>> room` markers; a line saved identically on both sides is kept once. Before, the room kept both versions one after the other without markers, so for code the second assignment silently overrode the first.
- `rbx collab` shares a link added while the session runs together with its existing local source (a file, a folder or a package): a saved `renium-link.json` refreshes the shared inputs before the save is filtered, the files it adds are published at once, and the manifest is watched before it exists. Before, the first save of a new manifest published nothing, a second save published only the manifest, and a source that already existed never reached the other participants.
- `rbx bs` refuses to edit an instance inside a read-only link, as `rbx rm` and `rbx upl` do ("Cannot edit read-only link ... Use --override-packages to replace it intentionally."), and takes `--override-packages` for an intentional edit. Before, it rewrote the read-only copy without a word and the next `rbx lk` undid the edit.
- A file added inside a read-only package copy (a new script under the linked folder) is drift: `rbx lk --check` counts it and names it (`"drift": "stray file ..."`), `rbx lks` marks the target `drift`, and `rbx lk` refuses until the file moves out. Before, all three ignored it until the package itself changed, while the projection already treated the script as part of the read-only copy.
- On Windows, `rbx wally --wally-path NAME` (and `--git-path NAME`) for a tool that does not exist reports `Failed to launch wally install using command "NAME"`. Before, Renium handed `NAME.cmd` to cmd and reported a failed install with cmd's "'NAME.cmd' is not recognized" text.
- `rbx plugin check` and `rbx plugin install` refuse a plugin guide over 64 KiB or one that is not UTF-8 text ("The plugin guide AGENTS.md is 71680 bytes; a guide can be at most 65536 bytes (64 KiB)"), the limit `rbx plugin info` reads with. Before, install accepted such a guide and `info` then failed with "AGENTS.md exceeds its size limit".
- `rbx collab` shares the project's links: `renium-link.json` and the link sources it names inside the project (`links/*.renium` packages, linked files and folders) reach every participant, and a link added later starts syncing too. Before, guests received neither, so their `rbx lks` showed no links and their read-only copies had no protection.
- `rbx upl` with Studio open while Live Sync waits for a decision unlinks the files too, then has Live Sync compare again; the answer's `liveSync` says whether the review cleared. Before, it answered that Live Sync would pull the unlinked package although Live Sync was paused, and `rbx lst --wait` kept the old review after both sides had dropped the link.
- `rbx upl` at the root of a shared link copy works in the places that use it, as `rbx lkp --share` tells them to: the Roblox PackageLink left out of the shared package is no longer protected as part of the read-only copy. Edits inside the copy are still refused.
- A pull or push verification that goes through the plugin's serialization (macOS, where the native capture is unavailable) keeps the script files the project already has: an existing `Packages/_Index/evaera_promise@4.0.0/promise/init.lua` stays a directory with an `init.lua`, and `Promise.lua` keeps its extension, as the native capture already did. Before, a module without children was written as `promise.luau` next to its directory, and `rbx ps instances/ReplicatedStorage.renium --yes` after `rbx wally` failed with "Studio did not retain pushed project changes: src/ReplicatedStorage/Packages/Promise.lua ... (src/ReplicatedStorage/Packages/Promise.lua differs from the requested value)" although Studio held the right instances, because the readback was named from an empty stage instead of the project's files.

## 0.5.7 - 2026-11-01

### Bug fixes

- `rbx wait` accepts a chunk with its own `return` (`local p = ...; return p ~= nil`) as well as an expression, and a timeout prints `{"ok":false,"timedOut":true,"value":...}` with the hint instead of a bare error. Before, a statement failed with a Luau parse error.
- `rbx rev --path FILE --print` falls back to the newest sync-history entry that holds the file when the editor history has none, `rbx rev --sync ID --path FILE --print` reads one file at a given sync entry, and the error with both stores empty names them and `rbx rev --sync latest`. Restoring from a sync entry stays whole-entry (`rbx rev --sync ID`), since only the whole entry carries the newer-edit guard. Before, a file with 81 sync entries got "No editor revert history found".
- `rbx tr StarterGui.Main.HUD.Timer` (one dotted argument) takes the first segment as the service when it names one. Before, the argument split at the last dot and failed with "No synced Renium store for service 'StarterGui.Main.HUD'".
- `rbx lon --wait N` waits for the first comparison and the watched file changes to settle, as `rbx lst --wait` does, and reports the settled daemon status; before, the flag was accepted and ignored.
- `rbx lst --prefer`, `rbx lst --clear-pending` and the other `lon`-only flags are rejected on `lst`, `lof`, `rp` and `dp`, whose `--help` now lists only their own flags. Before, `lst --prefer studio` was silently ignored and `lst --clear-pending` discarded pending edits.
- A command whose daemon response deadline passes reports the daemon as busy with the operation it is serving and how long that has run, and says that Studio operations run one at a time; a connection the daemon closed mid-request says so and asks for a retry. Before, both were raw socket errors ("Waiting for daemon response to operation 54 (43s limit) ... os error 10060"). `rbx status` already reported busy daemons; every command does now.
- `rbx ck` names a file it cannot read and a source that starts with a byte order mark, instead of "os error 123" or "got Unicode character U+feff".

### Improvements

- A request that only looks at Studio (`rbx l`, `rbx lc`, `rbx co`, `rbx sc`, `rbx ui`) waits at most 5 s for the Edit window's request gate and then answers that Studio is busy, naming the longest-running operation and how long it has run, with `retryAfterMs` in the error detail. Before, a second agent's `rbx l` queued behind a push or sync until its own 43 s socket deadline. Pushes, syncs and `rbx wait` keep queuing as before.
- Native replacement captures and Studio change-journal records in the plugin reuse one sibling-ordinal snapshot per traversal, so a push into a folder with thousands of same-named siblings no longer rescans the parent for every instance (1,500 siblings: 3 scans instead of 3,001).
- `rbx wait COND --result LUAU` and `rbx play -s --until COND --result LUAU` evaluate the result expression once after the condition holds and return it as `result` with tables intact (the serialization `rbx l` uses), so the state an agent waited for no longer needs a separate `rbx l --server` read; a failing result expression reports `ok:false` with `conditionMet:true`. The Play form evaluates on the play server only (`rbx wait --server` does the same) and refuses when the bound Studio changed after the launch or no server runs.
- `rbx ba --batch -` creates several saved instances in one run from `{"ops":[...]}` on stdin (or a file, or inline with `-J`): each op names `parent` (a dotted path from the service, a settings id with `service`, or `#ref` for an earlier op), `name`, `class`, and optionally `ref`, `source`, `props` and `attrs`; every affected store is read and written once; a validation failure writes nothing, a failed file write rolls back that store and the error names any store already written; the receipt lists each created id, path and source file. Property references in a batch must point at instances that already exist or at ones in the same store. An agent that scaffolded a feature with 13 separate `ba` launches needs one.
- `rbx help CMD` prints a command's help (`rbx help oc key` works for nested commands), `rbx log` and `rbx loff` resolve to `co` and `lof`, and an unknown command names the closest commands plus `rbx --help`, `rbx docs <guide>` and `rbx plugin list`.
- "Property not found" lists up to five nearest names from the instance's stored properties and its class, and names the listing command. "No matching instance" and the model export's "not found" name `rbx f`, `rbx tr` and `rbx pl`; a Studio that could not open its place names `rbx sx --terminate`; the play-client selector error names `rbx play --add-players`; the editor-transaction error names `rbx lst` and `rbx lof`; an unsupported key says which names work; `cfg` names `rbx cfg list`; Luau errors about `Source` or "is not a valid member" carry the commands that read scripts and list children.
- Text-mode results drop fields agents never read: `status` loses `availableDataModels`, zero `gameId`/`placeId`, `selected` for a single Studio and the diagnosis details behind its verdict, and lists a non-native operation as its name and fallback only; `cs` drops the bridge build, launch and port plumbing; `lst` and `lon` drop `role`, default `onlyCodeMode` and `twoWaySyncEnabled`, and the daemon members at their quiet values (`paused:false`, `syncing:false`, `settled:true`, zero counts); `play -x` answers `stopped:true` without the engine flag blob, `method` and `attempts`; `co` drops `epoch`; `q` drops `searched`; `cmp` drops the echoed `services` and the instance totals; `inp` drops the request echo and a null click count. Json mode (`--output-mode json`) keeps every field.
- The Windows release bundle no longer carries the POSIX `rbx` launcher script (the installers already dropped it in 0.5.6).

## 0.5.6 - 2026-10-31

### Bug fixes

- `rbx in` printed `{}` for every instance, and `rbx bb -F a,b` dropped its results: the global `--fields` option shares its name with the per-command `-F` of `in`, `tr`, `f` and `bb`, so clap handed those commands' field lists (presets included) to the global projection, which kept no key. A command with its own `-F` now renders its own fields.
- `rbx docs TOPIC` printed the README from the first line containing the word instead of the guide: `rbx docs sync` now prints `RENIUM/sync.md`; a README section still prints when its heading matches, and the error lists the guide names.
- `rbx ck` with no changed files reports `ok: true, checked: 0` instead of failing.
- The "RENIUM.md was updated" notice fired after every checkout on Windows, because the comparison counted line endings; it now ignores them and names the guide files whose text changed.
- `rbx lst --details` printed the placeholder "paths" instead of the counts, and `rbx lst --wait 240` failed instead of waiting the 120 s maximum.
- Store commands run outside a project (`rbx f`, `rbx tr`, `rbx in`) said the service had no store at a `src/<Service>` path; they now say there is no Renium project here and how to point at one.

### Improvements

- `rbx so FILE` and `rbx ro` wait until the place is connected (120 s, or `-w` when larger) before returning, report `connected` and `connectSeconds`, and fail with the diagnosis when the process exits or cannot open the place; `--no-wait` returns after the launch. A second `rbx so FILE` reuses the Studio that already has the place file open instead of opening a duplicate, and waits for its connection the same way.
- A Studio command with no Studio process running fails within a second with the diagnosis instead of after its connection wait, both in the CLI and in the daemon; a launch in progress keeps the wait.
- Windows installs no longer keep the POSIX `rbx` launcher script: Git Bash resolves `rbx` to `rbx.exe` by itself, and the script doubled the cost of every call (about 100 ms instead of 55 ms). `rbx setup`, `rbx upd` and `install.ps1` leave it out; `rbx.cmd` stays for cmd.exe. A caller that ran the script by path uses `rbx.exe` or `renium.exe` instead.
- The CLI's connection wait rebinds with backoff (50 ms to 400 ms) instead of every 50 ms, and the daemon logs binds at level 4; a wait used to add up to 500 bind lines and could hold the daemon's connection limit.
- The daemon keeps its decoded store cache (bounded by its own entry, instance and byte limits) across idle periods and drops only payload and projection caches after 5 minutes, so the first export after a pause stays warm.
- A push held for review in Studio says so on stderr, names `rbx rv apply`, `rbx rv skip` and `--yes`, says that an unanswered review is skipped after 90 s, and the `ps --help` text explains `--yes` against `--no-review`, which also disables protected writes.
- Rejected push entries log the first three at the normal level and the rest at level 5.
- The guides state the first-connection rule (pull once for a new or imported project, `lon` only for an existing one), the Studio push review, that `lon` returns before the first comparison settles, the import sequence under Live Sync, and the `so`/`ro` connection wait; `init`, `dr`, `docs`, `so`, `ro`, `status` and `sx` gained their missing flag help, and `rbx ip --help` shows script imports instead of a model file.

## 0.5.5 - 2026-10-30

### Bug fixes

- A Live Sync pull no longer overwrites a project file whose edits Studio has not received: files pending a push, or blocked by a failed push, keep their content and stay pending, and the daemon log names them. Before, a push batch that Studio rejected was followed by a pull that replaced the edited store with Studio's copy, so a model imported with `rbx mip` vanished from the files with nothing to undo.
- A write of Studio content into the project files now leaves a sync history entry: Live Sync pulls, `rbx pull`, and the files a reconcile (`rbx lon`, including `--prefer studio`) resolves toward Studio. `rbx rev --sync latest` restores the previous file content, as it already did for pushes. The entry is written before the pull reports completion; when it cannot be written, or a file was saved again in between, the replaced files stay in the export's recovery folder and the daemon log names it.
- A Live Sync pull that keeps a file back while Studio also changed its instances sends that file's next push through the three-way comparison, so the Studio edit is reported as a conflict rather than overwritten. A pull that fails on the current files (a store the daemon cannot decode) is retried with backoff once Studio reports again; before, pulls stopped until `rbx lof` and `rbx lon`.
- Live Sync's incremental pushes promote a heavy delta to a native service replacement under the same rules as `rbx ps` (2,000 or more new instances, or 64 or more engine-set field writes that outnumber the replacement's own; not for projects with files-to-Studio rules, a link manifest, or a service whose same-named top-level roots hold a package). The push then plans against the whole project, so the service's unchanged scripts and references into it from other services come along. Before, importing a 10,000-instance model into a store while Live Sync ran applied it instance by instance.
- The Studio plugin creates same-named siblings in the order of their store ordinals. Before, it sorted them as text (ordinal 11 before 2), so a push of more than 5,000 new instances whose parents hold many same-named children (a props folder of `Model`s) failed its second batch with `Cannot create instance; parent path was not found`, and the surviving siblings sat in another order than the store's.

## 0.5.4 - 2026-10-30

### Removed

- The Playtest teleport feature: Renium no longer intercepts `TeleportService:TeleportAsync` in Studio Play, loads destination places, hooks Studio's play launch or test clients, or ships the `ReniumTeleport` adapter. `rbx play` results no longer carry `teleport`, the `playTeleportEnabled` and `playTeleportNative` settings are gone, `rbx native-probe` lost its `--playtest-*` flags, and the `RENIUM/teleport.md` guide is removed. Play, Play Here and Server & Clients are unchanged.

### Improvements

- A native service import now writes the engine-set fields of the imported instances (a SurfaceAppearance TexturePack, a mesh id Studio's reader dropped) in the same push, in a short follow-up transaction once the import has joined its service; before, the packs kept their old values while the push reported verified, so a second push was needed. The result carries `importedEngineFields` with the row count.
- A store push whose delta is heavy (64 or more engine-set field writes in one service, counting a new MeshPart as three, a new SurfaceAppearance as one and a changed mesh field on an existing part as one, or 2,000 or more new instances) now replaces that service's authored contents through one native import instead of applying the change instance by instance. Before, a six-service push of a cloned game (about 4,000 new instances, mostly car meshes) queued about 2,000 native property writes one at a time, hitched Studio for two minutes and then failed; the same end state now lands in one deserialize. Instances of a replaced service get fresh engine identities, as in a full replacement push.
- `rbx ps STORE -i ID` now pushes the targeted instance with its whole subtree, so a newly added model arrives complete instead of as an empty root; a script ID selects the script alone, and a push that names properties keeps to the targeted instances.
- `rbx mip` says in its help that it writes the project store only, the configuration guide names the push that applies the import in Studio, and `rbx lst --wait` states its 120-second limit.

### Bug fixes

- Native property writes (MeshId, CollisionFidelity, TexturePack and the other engine-set fields) no longer fail a whole push with `Studio protected property call failed (0xE40F)` when a busy Studio frame skips the queued engine task: the engine call gets its own budget after discovery and history registration instead of their leftovers, the recording token is registered with Studio once per transaction instead of before every write, and a skipped task is retried once with a fresh target lookup.
- A native service replacement writes references from other services into the replaced service again, as after any recreation; before, an ObjectValue elsewhere kept pointing at the removed copy and nothing reported it. The replacement is skipped when the project disables native imports (files-to-Studio rules, a root `$ignoreUnknownInstances`, staged mounts, linked packages) or the service holds a package, and it is not chosen when it would cost more engine-set writes than it saves or when the changed fields are under one percent of the service.
- The extension's properties panel stores a script's Enabled flag under its canonical name, and the explorer dims a disabled script again (stores spell the flag `Enabled: false`).
- A native service replacement that followed an incremental push of the same instances no longer fails at commit with `Refusing editor mutation outside service root: CoreGui.Renium Transaction...`: the plugin resolved a post-import property row through the identity it had bound during the earlier push, which now pointed at the outgoing copy parked in the transaction's staging folder; an identity outside its service is skipped and the row resolves by path to the imported instance.
- A script that is disabled in Studio while the project files say it is enabled is now re-enabled by a push. The comparer treated a record without `Enabled` as saying nothing, so the push skipped the script and then failed its own retention check with `CaseHandler.Enabled was not retained (expected <absent>, Studio has false)`.
- `rbx lon --prefer studio` on a first pairing (no stored baseline) left the project's instance files unchanged while the new baseline took Studio's values, so the next Live Sync pass pushed the stale files into Studio and reverted Studio edits; the losing side is now written, and `--prefer editor` pushes from the document Studio actually holds.
- Every successful command on a place whose Live Sync is waiting for a decision now carries a `syncReview` line naming `rbx lst --details` and `rbx lon --prefer studio|editor`; before, the pending review showed only in the Studio widget.
- A pairing whose stored baseline points at deleted reconciliation packs no longer fails Play or Live Sync with `Failed to read reconciliation pack`; Renium drops the stale baseline, logs it, and the next pairing rebuilds it.

## 0.5.3 - 2026-10-27

### Improvements

- Text output is leaner where agents were re-parsing it. `rbx q` prints each match with a dotted path (`Workspace.Map.Pad[3]`) and without the repeated `name` or the input path; `rbx cmp --full` prints one line per difference (`changed Script ServerScriptService.Main: Source`) unless `--values` asks for the records; `rbx lks` prints one line per shared link with its places grouped by state; `rbx lk` no longer lists every settings id per link (`changedSettingsIds` still carries them); `rbx l` drops `context` when the target was pinned; `rbx co` drops the epoch and per-line sequence numbers unless `--since-seq` is used; `rbx ps --verify` names a mismatched script without hashes; a play-client selector error lists role, name and pid per bridge. `--output-mode json` keeps every structure.
- `rbx co` prints a header line and then one `[level] message` line per entry in text mode, the shape `--follow` already used, instead of a JSON object per entry; `--output-mode json` keeps the objects with `time` and `seq`.
- `rbx status` reports the native probe as verdict counts plus the operations that fall back, instead of every probe's readings (about 3,000 tokens on a large place); `--output-mode json` and `rbx native-probe` keep the readings.
- `rbx cmp --full` takes `--class CLASS` and `--props NAME,NAME` to compare one class or a few properties, engine-managed ones such as PhysicalConfigData included. Every command takes `--fields KEY,KEY` to print only those keys of the result and of each record in it. `rbx oc` help lists the actions before the shared options, and `rbx oc` results omit null and empty members in text mode.
- Action results are shorter in text mode: `rbx play` reports `serverReady`, the client count (names with `-p`), the teleport mode and the `--until` value instead of every launch identifier and bridge record; `rbx sc` reports `moved` and the capture size, omitting the window, device and the path the caller chose; `rbx wait` no longer echoes its condition; `rbx publish` reports the version numbers, `published` and `liveSync`; `rbx ck` counts checked files and lists only failing ones; `rbx oc fetch` omits the place name and byte count.
- Open Cloud errors print the status, the server's title and its field messages (`Open Cloud request 0 returned HTTP 400: One or more validation errors occurred; repository: ...`) instead of the whole response with its rate-limit headers; `rbx oc routes` without a category lists the categories with their action counts.
- The root guide says results are one JSON line on stdout and errors go to stderr, so agents stop adding `2>&1` and grep filters; the data guide documents the `q` record and the `cmp` line.
- The publishing guide says that a collaborator in Team Create is no reason to stop a Studio publish: the open draft is shared, and `rbx publish` ships it with their work.

### Bug fixes

- Native Play verifies callback layout and ownership before arming, supports Studio's compact continuations, and preserves running tests when a new multiplayer launch is refused.
- `rbx dev` and the Play start no longer cancel a device-simulator worker that Studio 0.745 has left suspended inside an engine call; cancelling it closed Studio. The call still answers within 1.5 s and the late return is ignored.
- The daemon no longer sends a device-simulator stop to every Studio that connects; device state is applied only after `rbx dev set` or `rbx dev stop` asked for it in this daemon session. On Studio 0.745 that unrequested call closed Studio whenever a device was active.
- `rbx play` starts even when the device-simulator status call stalls; the result carries the error instead of the device fields.
- The Server & Clients preflight error names the native probe's reason (an unpublished place, a failed structural check) instead of the generic native-hook sentence.
- Commands keep local Studio projects and explicit `--place` selections separate, including projects whose files share the same name.
- Large plugin fallback exports and sync batches yield regularly to keep Studio's controls and built-in plugins responsive.
- Server & Clients teleport now reports an unavailable native hook before starting the test and explains that compatibility travel supports solo Play only.
- Device simulation reports stalled Studio service calls promptly, so a failed device change does not leave later bridge requests waiting indefinitely.
- Automatic Play teleport setup stops retrying a known unavailable native hook and gives stalled Studios several minutes to recover after a timeout.
- Restore native serialization on Studio 0.745 so Live Sync and multi-client playtest setup can use the native path again.
- The daemon removes the control key files of daemons that have exited, instead of leaving one `control-PORT.key` behind in the private folder for every port it ever used.
- `rbx lk` and `rbx wally` report changed settings ids as `Service/ID`, and `rbx push -I` keeps such an id to that service; a bare id still selects that instance in every store. Ids are numbered per store, so a push given the 70,000 ids of thirteen links also matched same-numbered instances in other services, pushed instances outside every link target, and failed on a Wire whose SourceInstance pointed at an instance that is not in Studio.
- Large pushes no longer fail with "Renium request lease was cancelled" while automatic playtest teleport setup runs.

## 0.5.2 - 2026-10-25

### New

- Solo Play on Windows and on Apple silicon macOS supports TeleportService travel to saved destinations with native server/client models and join data. Travel uses Roblox's default loading screen, updates the window title immediately, and prepares the next launch before leaving the source. Stop restores the original unsaved Edit. A restricted compatibility adapter remains available when native switching is unavailable.

### Improvements

- Server & Clients tests connect even when the server never reports its place as loaded.
- Studio recordings keep the whole window across Play and teleport transitions.
- Commands that change the open Edit window (`rbx l` in Edit, `ps`, `prop`, `del`, `me` and `ai`) now answer with `persistence`: `teamCreate` when the place is in a Team Create session, which syncs the change to Roblox by itself, or `unsaved` when it stays in the Studio session until the user saves or publishes. `rbx status`, `lst` and `lon` report the same field, and every connected Edit window in `status` and `cs` carries `teamCreate`, so it no longer has to be inferred. A plugin that predates the field is asked once per Studio. An agent had added an anchored part to a Team Create place with `rbx l` and told the user to save the place; the guides now say never to ask for a save when `persistence` is `teamCreate`.
- A project bound to a local place file now selects the Studio that `rbx so FILE` opened when several Studios are connected, instead of refusing with "More than one Studio runtime matches this project" until `--place` is passed. `rbx so` records the opened place beside the project files (as `rbx sx` already did), and that record only breaks ties: a lone Studio is still selected as before.
- `rbx q --props` and `rbx bg` answer a legacy property name with the value Studio saved under its successor: `Image` on an ImageButton or ImageLabel returns the `ImageContent` asset instead of "not saved" or "Property not found", which sent an agent off grepping compressed files for asset ids.
- The Studio panel's sync line reads "Syncing" while Studio edits wait for the daemon, instead of "Last synced at ...", which read like a different state from the "Synced at ..." it turns into a moment later.

### Bug fixes

- A native capture that found one instance more or fewer than Studio had counted a moment earlier (a collaborator's edit replicating during the capture) is now retried like any other change that lands mid-capture, instead of failing the command that triggered it. An `rbx l --edit` on a Team Create place had been refused once with "Studio native Workspace snapshot contains 14632 instances; expected 14633" and worked on the next try.

## 0.5.1 - 2026-10-23

### Improvements

- When Studio cannot place a pushed instance, the error now names the candidates Studio compared and which values differed, or why the instance at the previous path could not be used (missing, claimed by another entry, or of another class), and the daemon log lists the entries at that path with their previous locations and identities.
- When a pull or link apply refuses to replace a linked copy because another store still references an instance inside it, the error now names the link, the instance's path and the referring instances instead of a bare store ID.

### Bug fixes

- Live Sync's push after a comparison now resolves each Studio instance by the identity Studio's own capture gave it, and only then by its previous path. The capture lists same-named siblings in another order than Studio does, so a group of twins (seventy same-named parts under one model, say) could be told to replace the wrong siblings and Studio refused the whole push as unidentifiable; the push plan also follows the comparison's pairing instead of pairing Studio a second time on its own.
- A store that reuses an identity for an instance of another class, as a replaced subtree numbered from the store's size can, no longer makes Live Sync treat the old instance as changed into the new one: the identity is released and the two merge as a removal and an addition. Before, a replaced model pushed as hundreds of class replacements and moves.
- A pushed instance the comparison found no Studio counterpart for is now created instead of matched to a same-named sibling nobody claimed, which refused with "unmatched siblings" when the leftover siblings differed.
- A pushed CFrame compares against what the engine stores back within the same noise the comparison allows (2e-4 studs, 9e-6 per rotation component) instead of f32 rounding; the engine re-derives a written CFrame, and the stricter check failed the push with "Roblox did not retain CFrame" and then failed its rollback. Anchored is written before CFrame so a welded part is not dragged off the written value, and when two siblings match a request within that noise the one matching exactly is chosen.
- A union, negation or intersection nested in a model whose geometry changed is removed and imported anew rather than edited in place; its CollisionFidelity values Box and Default compare as one value and a write between them is accepted as the engine's own state (it reports Box for a box placeholder, Default once cooked, and honours neither write), while Hull and PreciseConvexDecomposition remain real changes; a Union's collision cook is awaited like a MeshPart's. A push of a model with such operations had failed on the fidelity write.
- Two collision hulls that encode the same fidelity for the same mesh compare as equal in every comparison, and the box placeholder Studio saves before a mesh has cooked matches the hull the engine later cooks from it, so a push that makes the engine cook hulls (a native import of a service) is no longer reported as not retained on an untouched MeshPart, and a pull keeps the files' hull bytes while the mesh and fidelity agree; a hull is never carried across a mesh change.
- Live Sync no longer reports a PackageLink as removed when neither Studio nor the files hold it any more, which blocked a place that shared a copy whose link the consumer had dropped on purpose.
- A pull or link apply no longer refuses to replace a linked copy because of a reference that resolves inside the referring store itself; observation identities repeat across stores, and the check read them as references to the copy. A reference that names another service explicitly still counts as reaching into it.
- A pull's read-only link restore no longer writes empty `.luau` files over a consumer's shared scripts: a store record that carries no Source takes the package's text for that script, or keeps the file that is there (a pull keeps script sources aside, which emptied up to 489 files per place and shipped empty scripts in builds made right after a pull).
- Live Sync no longer reports a copy's deprecated `CollisionGroupId` (the engine's per-place index of the collision group name, `-1` vs `3` on a shared-link copy) as a difference; the `CollisionGroup` name still compares.
- Live Sync preserves completed commit/rollback results across retries and releases inactive failed transactions after bounded retries or Studio ownership changes. Recovery has at most eight scheduled opportunities under valid ownership; unresolved data is session-only, with cleanup scheduled 30 minutes after retirement once current work permits. Native preparations delayed 120 seconds are safely refused; authorized writes with unknown outcomes stay blocked until verified completion or Studio restart, which can lose retained data.
- Live Sync keeps its baseline when the project or experience configuration changes (a shared link applied with `lkp --share` or `lk --experience`, a place alias added). The next comparison is still a full one, but the files that change wrote are edits against the baseline instead of conflicts between both sides, so a consumer no longer holds on every transform of a freshly applied copy.
- Live Sync's first comparison no longer holds on Workspace's `InsertPoint`, transient editor state that Studio moves with the camera and never saves.
- Offline commands (`ck`, `report`, plugin calls) no longer try to resume Studio audio, and a failed resume is logged at debug level instead of being printed into a command's output; a sandboxed `rbx ck` had printed "Global Studio audio could not resume: Access is denied".
- Live Sync pairs reordered same-named siblings and their joints without losing additions or changing known instance identities, including unequal groups of reference-bearing twins that previously conflicted or stopped reconciliation. Renames, moves and references to changed twins remain conflicts; unchanged nested references and Studio package metadata updates no longer cause false conflicts, and repeated references stay fast in large places.
- Live Sync's first comparison no longer reports a fresh `pi` project as differing from its own Studio: tags compare as a set, an asset given as `rbxassetid://N` matches the `roblox.com/asset/?id=N` form Studio hands back, a CFrame or WorldPivot Studio recomputed within 2e-4 studs or 9e-6 in a rotation column (a rotation under about 0.0005 degrees) of its saved value is unchanged, a GuiButton's `InputSink`, which the native capture leaves out, and a world gui's `ResetOnSpawn`, which Studio reports as false while the saved place carries nothing, say nothing when one record lacks them, a WeldConstraint's `State` is engine state, and a MeshPart's collision hull compares by the fidelity it encodes rather than by the engine's bytes in that first comparison, while merges and pulls keep the bytes Studio has. The conflict description now lists only the members the comparison counts as different, instead of every default present on one side. On a scratch place this turned 898 "different values" lines into none.
- A MeshPart recreated by a push keeps its collision fidelity: Roblox stores CollisionFidelity only inside the cooked hull, so a store record with a hull but no fidelity now hands the plugin the fidelity the hull encodes (Hull, Box, PreciseConvexDecomposition) instead of letting Studio cook a Box placeholder. Before, cars moved between projects with `mep`/`mip` lost their hulls on push.
- Live Sync can start when Studio briefly changes context during a busy edit session. It retries briefly, then uses Studio's export fallback if native capture remains unavailable, and reports the attempt count and fallback in the startup result.
- Live Sync and pulls keep using fast native capture after compatible Studio layout changes, preserving native identity and engine-only fields. Unsupported layouts now report a clear compatibility reason and use Studio's export fallback without repeated capture refusals.
- Store IDs as `rbx find`, `bb` and `v` print them (plain hex such as `a8d7`) now work as parent selectors in `mip`, `ba`, `cr` and every other command that takes an ID or a path, including stores written by `pi`; before, only IDs containing a colon were tried as IDs and the rest were read as paths, which failed with "No matching instance path".

## 0.5.0 - 2026-10-21

### New

- `rbx report --upload <ID>` re-zips a written report folder and sends it to the Renium developer's report store: a Cloudflare Worker that accepts only the archive shape `rbx report` writes (report files only, every entry inflated and checksummed under size caps, UTF-8 text, a `report.json` whose ID and file list match), stores it in R2 once per ID within per-address, global and daily budgets, and answers with the ID it was filed under, so "report RNM-20261021-AB12CD" is enough to find the bundle. The new `RENIUM/bug-reports.md` guide makes agents read every file in the folder and remove anything private before uploading; files outside the report layout are left behind, and a refused or failed upload keeps the zip for a manual attachment. `report.json` now carries the message.

### Improvements

- The Studio lifecycle guide says to open Studio only with `rbx so` or `rbx ro`: Renium launches it behind the user's windows and blocks its self-activation, which a raw `Start-Process` or `Popen` launch cannot, so such a Studio takes the foreground after loading.
- Recordings default to 60 FPS and accept `--fps` up to 60. `re` reports `requestedFps` and `achievedFps`, and `throttled: true` with a note when the window delivered well under the requested rate, which a minimized or fully covered window does; a capture names a window it restored from the taskbar. The capture guide says to keep the recorded window restored behind other windows and never minimized.

## 0.4.9 - 2026-10-21

### New

- Places of one experience can share a subtree. `rbx lkp --share` in the place that owns it packs the subtree, scripts, UI, models and attributes together, into `links/ID.renium` at the experience root, records it under `sharedLinks` in `renium.experience.json`, and links every other place that has the same path to it read-only. `--all-places` also creates it where it is missing; other places are listed as skipped. The source place stays editable, and the pack never rewrites its files.
- After an edit in the source place, `rbx lk` in any place of the experience, or `rbx lk --experience` from anywhere inside it, re-packs each shared subtree whose source changed and updates every place that follows it. With Live Sync running in the source place this happens on its own once the edit settles, and each following place's Live Sync pushes the new copy to its Studio; a failure shows in the Live Sync status instead of stopping it.
- `rbx lks --experience` lists every shared link with its source place, whether the package is `stale`, and each following place as `ok`, `drift` or `missing`; `rbx lks` inside an experience adds the same `shared` section.
- Places that follow a shared link get the scripts as read-only files under the shared root, as in the source place, and files there that the package does not contain are moved to `.renium/link-replaced/ID/` (listed as `replacedFiles`, counted per place as `replaced`) instead of stopping the update.
- A Roblox PackageLink directly under a shared root is left out of the package and reported as `strippedPackageLinks`, together with the `rbx upl` command that unlinks it in the source place. Packages nested deeper, such as TopbarPlus, travel unchanged.

### Improvements

- Team Create reverts a script reparent out of Workspace.Terrain within a second, so a pull right after shows the old parent; the guide now says to move such instances with Clone plus Destroy.

### Bug fixes

- `ip --path-json` registers the script in the service store when the project is store-backed (a `pi` import), so `bep` and pushes include it; the result shows `storeInstance`. Before, the file was written and silently left out of builds.
- `pi` and model imports keep `SurfaceAppearance.SurfaceSlot` even at its default, so a build round-trips the API-limited type byte for byte.
- Applying a shared link into a place whose store happens to reuse a settings id no longer fails with "Ref selectors mix local and nonlocal targets"; a reference whose path names another service stays an outside reference, and the error names the reference when it does occur.
- `cmp --full` pairs same-named siblings by content when their subtrees swapped places (two car models with one name, a folder of `Part`s), including models that reference their own parts, reporting `reorderedSiblings` instead of a changed entry per twin; groups above 512 twins keep positional pairing.
- A push that cannot set a part's CFrame because a Weld, Motor6D or WeldConstraint holds it says which joint, and a rollback no longer fails on such a part when the engine already holds the journaled value.
- "Reconciliation state has unresolved changes" now names the ways out: `lst --details`, `lon --prefer studio|editor` (which also pushes the other side's one-sided edits), or `lof` followed by a scoped push.
- An unreviewed push logs the instances it deletes and the services it reconciles (where Studio-only instances are removed), with `-u` as the way to keep them.
- Every Open Cloud place-version upload (`publish --open-cloud`, `publish --as`, `oc place publish FILE` and raw requests to that endpoint, Saved or Published) validates the exact bytes it sends first: nonempty, at most 100 MiB, parseable binary or XML, and no known floating-point member written with the wrong width. A place built with `Camera.OrthographicSize` saved as a double, which Roblox servers could not initialize, is now refused before upload with the member named. `publish --open-cloud --file FILE --dry-run` runs the same checks without credentials, and `--saved` now also works with `--open-cloud`.
- Reading a place whose float members were written as doubles by an older build keeps their values instead of misreading the bytes.
- Pulls store known numeric properties as native records again; an import followed by `bep` no longer resets saved values such as `WrapTarget.Stiffness`.
- The publishing guides describe the Open Cloud limitations for EditableImage, EditableMesh, unions, SurfaceAppearance and wraps, the verification an agent owes before uploading, and a Studio-free engine check through Luau Execution.
- Read-only link mirrors are no longer reported as differing from their packages after every pull, and `lk --check` no longer reports drift for them. Engine identities (UniqueId, HistoryId, ScriptGuid), fields Studio recomputes after a load (collision hulls, aero data, migration flags, cached mesh bounds), `RBX_` attributes, values at the class default, typed-number wrappers, script line endings, tag order and the order of differently named children no longer count as differences; references compare by path below the shared root; and a subtree without a placement anchor (a folder of scripts) compares like any other. Lock fingerprints written by earlier versions are not read as edited targets.
- `rbx build`, `bep`, `mep` and publishing from files leave out a numeric member the bundled reflection database does not know and name it once in the log, instead of writing it with a guessed type; pulls keep such a member's engine type in the stores so a later build writes it correctly. Studio refused to open a place built with `Camera.OrthographicSize` saved as a double.
- The bundled Roblox reflection database is regenerated from Studio 0.742 (it was built from release 740), so the classes and properties added since then export, import and build with their real types and defaults.
- Pulls restore ordinary and shared read-only package mirrors only in the selected services, and report them under `readOnlyLinksRestored` with the first differing instance and property under `difference`. Restoration is included in the pull's safe publication, sourcemap and Live Sync tracking; failures keep the original files and pending changes. Shared stray files are preserved without replacing earlier backups, and an intentional override push keeps its result.
- `--place NAME` no longer fails for every Studio when one other Studio momentarily refuses its window-name query (a Studio busy with a heavy batch); the read is retried briefly before it counts as unavailable.
- `rbx push --verify` only compares script sources, including with no paths or a whole service selected. It does not prepare a review, replace Studio content or acknowledge pending Live Sync changes. `--verify-sources` remains push-then-check, and publish dry runs also bypass review.
- Package checks, status and read-only push guards agree on external script edits, references and model placement. Checking many links in one large service avoids repeatedly copying the whole store; shared-link status also reports stray files.
- Push verification accepts omitted properties at their defaults and ignores Studio's insert point, computed layout/media readbacks and reserved engine attributes. Authored attribute changes and deletions still require exact retention.
- Native Studio features now check their own live signatures, reflection bindings and allocator contract instead of refusing every feature after a Studio version change. `rbx status`, `rbx doctor` and `rbx native-probe --pid PID` report each capability and its fallback reason.
- Studio 0.742 history recordings and long property reads use Studio's allocator when strings or callbacks cross the native helper boundary, fixing the wrong-heap frees that caused `c0000374` crashes. Terrain subscriptions retain safe engine tables across helper generations.
- `rbx push --verify` checks ScriptDocument/source batches without applying an unfiltered place replacement or capturing a sync checkpoint, including when native capture is unavailable.
- A missing named daemon no longer connects silently to the default daemon. Native probes and named-daemon commands leave global audio and recovery watchers alone.- Targeted pushes filter script creation, deletion and source updates before changing the store; unrelated scripts and read-only links remain outside the selection. Full-service selections still check every affected link.
- Model exports and native insertions avoid accidental box collisions. Imported Box hulls survive without the original place, comparison reports them, and base geometry recovery checks class and collision fidelity.
- Cloning scripts and importing models refuse to overwrite existing source files with different contents.
- Native Open Cloud upload commands accept `--content-type`; explicit fetch universe/place flags override bound project IDs, and failed universe lookups no longer silently download a different place.
- Export configuration and sync completion stay on the Edit runtime during Play; play commands preserve the Edit pin used by an in-flight sync.
- Camera capture readback checks viewing direction as well as position. Numeric property values accept a leading minus sign, and each agent guide stays within 5,000 characters.
- `rbx sc --camera-position -10,5,0 --look-at -20,0,0` accepts coordinates that start with a minus sign without the `=` form.
- `rbx sc` with `--camera-position`/`--look-at` reports the camera it found after the move under `camera` (requested, actual, moved, cameraType) and says when Studio did not hold it, so the capture needs inspection before use; `--studio` help says that during Play it captures the play view.
- A push by target id (`ps -i ID`) skips script files outside the targets. It used to send every script file in the project, so a script whose instance had been deleted in Studio came back with stub parents even though the push was limited to another subtree.
- `rbx ba` for a script class adopts a script file that already exists at the new instance's path (the result names it under `adoptedSource`) instead of truncating it to the empty source, and refuses a `Source` property that differs from that file; a missing file is created as before.
- A push limited to one subtree (`ps -i ID`, `ps PATH`) no longer refuses with "Cannot edit read-only link" because a shared link elsewhere in the same service has drifted in the files; the read-only check now covers only links the push can reach (inside or above a pushed subtree), for pushes by path and by target id alike, so a link in another service never blocks a push. Full-service pushes check every link as before.
- `rbx play -s --players 1` is refused with a pointer to ordinary Play (pass `--mode play` as well when a separate server with one client is really wanted); multi-client starts and restarts say in their result that a local server plus N client windows are running and how to end them. The playtest guide opens its Sessions section with the one-client rule.
- `rbx oc fetch NAME` with a project bound to a place fetched the bound place and ignored the positional target; the positional now wins (a number is a universe when one exists with that id, otherwise a place id), and a delivery response without a download location names the place and says the place or version may not exist.
- `rbx oc asset create` sends uploaded files with their media type (`.glb` model/gltf-binary, `.gltf` model/gltf+json, `.fbx` model/fbx, `.rbxm`/`.rbxmx`, audio, video and font formats) instead of application/octet-stream, which Open Cloud refused for Model assets; `--content-type MIME` overrides it for multipart files too.
- Live Sync no longer wedges on "Another editor transaction is already active" while a Play test is driven from the CLI. Editor calls (transactions, source reads, change state) routed through the default target, which prefers a play server during Play, and a concurrent play-runtime command (`l`, `lc`, `inp`, `pr`, `sc` against a server or client) cleared the shared runtime pins, so a commit or rollback could reach the play server and the transaction stayed open in Edit. Editor methods now always route to the Edit runtime, Live Sync pins only Edit, and play-runtime commands leave the shared pins alone.
- Built places no longer give meshes box collisions. `rbx pi` dropped MeshPart collision hulls (PhysicalConfigData) as engine-managed, and the place writer then filled every part that lacked one with the database default, Studio's `CSGPHS ... BLOCK` placeholder, so the built place told Studio those parts collide as solid boxes (invisible walls on Baseplate Drifting's tracks). The import now keeps the hulls; `--base` and the import base supply a hull for any part that still lacks one, from the same part in the base when its mesh and collision fidelity are unchanged, or else the one compatible hull the base holds for that mesh; a part with Box collision fidelity takes Studio's box hull; and when parts of a class remain without hulls the place carries none for that class, so Studio rebuilds them from the meshes on load (the build logs how many). `rbx cmp` reports a placeholder against a computed hull instead of filing it under engine-managed properties. UnionOperation ChildData2 and MeshData2 (the CSG tree and mesh) were dropped by the import the same way, which left unions empty in built places; the import now keeps every SharedString- and NetAssetRef-typed property (hulls, union trees, meshes and SolidMeshHolder, aero meshes, Terrain.Materials) and SourceAssetId, the base supplies any of them a part lacks by path or asset id, and the build's hull line explains that a class with any part lacking a hull is written without hulls (`rbx q` lists those values under `defaulted`).
- Place snapshots, including `rbx publish --as`, use the plugin export fallback when their native probe fails. Snapshot diagnostics identify the Studio build, window, process and failure reason.
- `rbx pd` and `rbx pu` no longer fail with "Could not read Studio memory" on Studio 0.742, and the automatic package desync works there. Studio 0.742 moved the slot that holds a property's native binding on its reflection descriptors; the package path read the old slot. The binding is now located by its runtime type name, as the protected-property path already did.
- Package ownership checks ask Roblox for the account's group list once per Studio session instead of once per package. Places with many group packages tripped the rate limit, which left ownership unknown.
- Package links that replace an existing subtree keep the package's internal references (a Model's PrimaryPart, ObjectValues inside the package). The reorder that puts the new copy where the old one stood rewrote reference indices from settings ids and dropped the index-only references the clone remap had just produced, so copies came back with empty references and every status compared them as drifted.
- Linked models that sit elsewhere in a following place no longer show permanent drift. Each side is now compared relative to its own placement and CFrame values may differ by float noise (0.02 studs, 0.001 in rotation), and the comparison no longer depends on child order. `rbx lk --check` names the first differing instance and property under `drift`.
- A Live Sync session started in verify mode now continues in reconcile once its first comparison finds no difference, so later file edits reach Studio. Verify used to hold every push for the whole session while `rbx status` reported a running sync with nothing pending. While verify does hold pushes (differences are waiting for a side), the `liveSync` summary in `rbx status` says `mode: verify` and `pushesPaused: true`.
- A settings-file lock left behind by a process that no longer exists is cleared as soon as it is met, instead of only after it is thirty seconds old. A daemon stopped mid-write (an install, a crash) could otherwise block `rbx lk`, pulls and pushes on that store with "Timed out waiting for settings file lock" for half a minute.
- Shared links apply cleanly to a following place whose store carries duplicate settings ids: the copy takes a fresh id where the preserved one is already taken, instead of failing with "Package identity mapping produced duplicate settings id". An empty BinaryString (an unset SerializedOverrides on a Model) no longer counts as a difference between a copy and its package.
- `rbx lks --experience` loads each following place's store the way the apply does, so it no longer reports drift on copies the apply considers current (it compared raw stores, whose references are index-based, against the package). It also names the first differing instance and property per drifting place under `drift`, and link comparisons work on the target subtree alone instead of cloning whole stores, which cost gigabytes and minutes on large places.
- `rbx upl` unlinks the Roblox package in the connected Studio (it destroys the PackageLink there, which is what Unlink from Package does) and lets Live Sync pull the result into the files. It used to edit only the project store, and Live Sync then refused to push a direct PackageLink removal, leaving the session blocked. With no Studio open it still edits the files alone.
- `rbx ps --verify` now compares the selected script sources with Studio and pushes nothing, reporting mismatches under `sourceVerifyErrors`; `--verify-sources` keeps its push-then-verify meaning. `--verify` used to be an alias of `--verify-sources`, so a command meant as a check pushed the files.
- In verify mode, `rbx lst --details` lists the paths Live Sync would move as `push:` (project side differs) and `pull:` (Studio side differs) instead of the bare "Studio and project files differ".
- `rbx lon --prefer studio|editor` reconciles for that run even when `liveSync.initialSyncPriority` is `verify`; an explicit side used to be ignored in verify mode.
- `rbx cmp` and `rbx bep` no longer fail with "property Properties cannot be represented in a Roblox model" on projects whose StyleRules set enum properties (a `SortOrder`, for example). The project store kept only the item name for an enum nested inside an attribute map or an array, so the enum type was gone when the project was rebuilt into a model; nested enum items now carry their type (a new store value kind, read by this version and later).
- `rbx lon`, `rbx lst` and the other Live Sync commands now honour `liveSync.initialSyncPriority` and `liveSync.initialConflictPreference` from the project configuration, and accept `--initial-sync-priority` and `--initial-conflict-preference` for one run. The CLI never sent these settings to the daemon, so `rbx cfg set liveSync.initialSyncPriority verify` had no effect from the terminal: the first connection still reconciled and pushed Studio-side differences into the place. Only the VS Code extension applied the setting.
- `rbx publish --as` builds its snapshot on the previous version of the target place when the native snapshot probe fails: the services the project does not track and the root fields the plugin cannot read (Players.MaxPlayers, Workspace.StreamingIntegrityMode, ...) come from that version, Studio's tracked services on top, and the result reports `base`. The snapshot used to carry only the tracked services with those fields at their defaults. `RENIUM_PUBLISH_SNAPSHOT=PATH` keeps a copy of the uploaded file.
- Pushes and Luau runs that add instances use binary import when the native service-reader probe fails, before attempting a native insertion.
- `rbx audio` validates the Windows audio render client independently from engine features before attaching its output gate.
- With `liveSync.initialSyncPriority` set to `verify`, a `rbx lon` re-run or a daemon restart on a pair that had already continued in reconcile turned the running session read-only without a comparison: Studio edits (a collaborator's Lighting change in Team Create) then stopped reaching the files, with no error shown. The pair now keeps reconcile unless something calls for a new comparison, which then verifies as before.
- Places built by `rbx bep`, `rbx publish` and model exports open in Studio again when a StyleRule has no properties set. Studio reads a StyleRule's property map strictly and refuses the place with "Failed to read StyleRule properties: Unexpected end of file" when the map is written without its zero count; exports now write the counted empty map, as Studio does.
- Places built by `rbx bep`, `rbx publish` and model exports open in Studio 0.742 again when they carry a saved integer field the reflection database does not know (SurfaceAppearance.SurfaceSlot, which 0.742 started saving). The stores keep such a field as an integer, but the export wrote it as a double, and Studio refuses the file with "Unexpected int format 5 (expected 3 or 27)". Untyped integers now export as Int32 (Int64 beyond its range).
- `rbx bep`, `rbx publish` and model exports fill a saved field the reflection database cannot default from the values the stores carry. A field the database does not know (SurfaceAppearance.SurfaceSlot, Texture.ResampleMode) or has no default for (GuiObject.InputSink) used to reach instances that lacked it as the writer's zero value, an invalid enum (4294967295) among them, once any instance of the class carried it. Empty unscriptable blobs (an unset SerializedOverrides) are left out, as Studio leaves them out of its saves, so they no longer spread to every instance of the class in the built place.
- `rbx pi` records the place it imported, and `rbx bep` and `rbx build` use that file as the base while it is unchanged on disk, so a build without `--base` keeps the services the project does not track (HttpService with HttpEnabled, AvatarSettings, PhysicsService, Chat, ...) and the root fields it cannot read. The result says `baseSource: import`, or carries a `warning` when the file moved or changed. The import also keeps instance `Capabilities`.
- `rbx pi` and `rbx oc fetch -r` keep the saved service fields the plugin cannot read (MaterialService's custom material names, Workspace.SignalBehavior), so a later `rbx bep` without `--base` carries them. They used to be dropped on import.
- Linked models placed elsewhere in a following place are moved by proper rotations: saved rotations drift from orthonormal by float noise, and a transform built from them spread that drift to every part and grew with each propagation (a pivot axis of 0.99998 after a few rounds).
- `rbx cmp` no longer reports an empty blob against none (Studio versions save an unset SerializedOverrides either way) or a CFrame moved by float noise (under 0.001 studs, rotation components under 0.00002) as a difference, and same-named siblings that swapped places pair with the subtree they came from instead of by position, so two models named Model under one parent no longer appear as hundreds of removed and added instances. A member without a reflection default that every instance of a class carries on one side and none on the other (a field a newer Studio version started saving) is set aside under `oneSidedProperties` instead of being reported on every instance.

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
