# Sync

`pl`: Studio → files (replaces the pulled files). `ps`: files → Studio. Live Sync handles both directions, and it is what a user who says sync or start sync asks for. First connection: `rbx lon` in a new project pulls every service; in a partly pulled one it pulls the services the folder lacks once the files it holds match Studio (a difference there waits as a conflict and nothing is written); a `pi`-imported project pulls once after Studio opens the place; an existing project runs only `rbx lon`.

```powershell
rbx pl
rbx ps src/ServerScriptService/Main.server.luau   # a push of up to 25 scripts reads them back: `verified: true`
rbx lon
rbx lst
rbx lof
```

`ps` pushes files or directories; for store edits pass the store file with `-i ID` per instance (with its subtree), or `-u` to upsert without deleting; deletions need the store file without `-i`.
A push, `prop` or `del` above `liveSync.changesThreshold` (default 5; 0 = all) applies nothing and returns `heldForReview` with the change list in `review`: check it, then rerun with `--yes`. Script files named on the `ps` command line are syntax-checked as `ck` checks them (an error stops the push) and apply without the hold; `ps -s SERVICE` pushes a service's store file. Editor and Live Sync pushes open a Studio panel instead (`rbx rv apply|skip`). `--no-review` also disables protected writes.
Work in the place folder or add `--place <alias|placeId>`.

## Live Sync

Start once for sustained editing. Saved files and Studio edits sync both ways. Unsaved buffers do not sync; file edits during Play wait for Edit mode.
`lon` returns while the first comparison still runs (`daemon.settled: false`); `rbx lon --wait 120` returns once it has settled, or `rbx lst --wait 120` later.

Trust successful edits while Live Sync reports no problem. Don't poll, push, reread Studio, or launch Play after every save.
Use `rbx lst --wait` (10 s, or a number up to 120) only when the next step needs completed sync, after a reported problem, or when asked.

On failure, inspect `rbx lst --details`, fix the cause, then retry with `rbx rp`. Failed edits stay pending; `rbx dp` discards them.
`lst` restores enabled watchers after a daemon restart; a running daemon alone is no reason to push.

## First connection and conflicts

`no_studio` and `rbx status` carry `diagnosis.verdict`: Studio closed or another place open means `rbx ro`; relay other verdicts unchanged.

Live Sync compares each side with their last common Renium state: one-sided changes transfer, independent edits merge, conflicts wait without overwriting either side. Something you added that both sides now lack was deleted in Studio and synced; ask before restoring it.

A place closed without saving can reopen older than the files: set `rbx cfg set liveSync.initialSyncPriority verify` before `rbx ro`, then read `rbx lst --details`.

`rbx lon`/`rbx lst` list open conflicts as records: `id`, `kind`, `path`, `instance`, `field`, values `base` (last sync), `files`, `studio` (scripts: `filesDiff`/`studioDiff`), `absent`, evidence `filesChangedAt`/`studioChanged`. Decide each from its values: `rbx lon --resolve ID=studio|editor`, repeated, all in one run (any conflict left open: nothing applies); `--prefer studio|editor` decides all. Keep the side whose work you can account for: `studio` after just opening the place, `editor` when the files hold your edits; ask the user only where both sides hold work you cannot account for. `decidable: false` (PackageLink edits, ambiguous pairings): fix by hand. Decisions settle only what both sides changed; one-sided changes go over. Files you cannot account for (older copies, foreign folders): ask, or take Studio's service with `rbx lof`, `rbx pl -s SERVICE`, `rbx lon`; that pull removes scripts Studio lacks, and `rbx rev --sync ID` with its `historyId` restores them.

`rbx cfg set liveSync.initialSyncPriority reconcile|verify`: `reconcile` applies the comparison; `verify` lists paths in `rbx lst --details` as `push:` (files differ) or `pull:` (Studio differs) and holds pushes and pulls (`pushesPaused`) until `rbx lon --prefer studio|editor`; without differences it continues in reconcile. `liveSync.initialConflictPreference`: `none`, `studio`, `editor`.

## Manual sync

Without Live Sync, pass files or directories to `ps`; Renium batches them by service.
For store edits, use returned changed paths and settings IDs (`Service/ID`, since IDs repeat per store) to keep the push scoped.
An unfiltered push reconciles the entire place and can remove Studio-only content.

Pulls restore read-only `.renium` link mirrors from their saved packages (`readOnlyLinksRestored` lists them; a difference does not tell which copy is newer). Push the link path to apply the package in Studio, or edit the source and run `lk`. Restores are not user edits in Live Sync. `ps --override-packages` bypasses push protection; a later pull restores the package again. See [links](links.md) for stray files.

`ps --verify` compares script sources and pushes nothing (`sourceVerifyErrors` lists mismatches; all scripts without paths). A push of up to 25 changed scripts reads them back on its own (`sourceVerified`, `verified: true`); `ps --verify-sources` forces that for larger pushes. Reads of `Instance.Source` are no substitute: an open ScriptDocument can differ.

Player capacity (`Players.MaxPlayers`, `PreferredPlayers`) comes from Roblox Game Settings; exports and comparisons keep it.

## File-backed undo

Syncs save their pre-sync files in `.renium/editor-history/sync`. `rbx rev --sync latest` restores the last sync's files (a returned `historyId` selects another); it refuses newer edits and unconfirmed transactions. `rbx rev --path FILE --print` prints a saved copy (editor history or the newest sync entry; `--sync ID` picks one).

Live Sync transfers restored files; otherwise add `--apply-studio`. `--details` lists every restored path.

## Packages owned by others

Pushes into Roblox packages this account does not own are refused. Revert to the package's content, or unlink it in Studio (right-click the root, Unlink from Package).
