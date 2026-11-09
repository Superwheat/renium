# Sync

`pl`: Studio → files (replaces the pulled files). `ps`: files → Studio. Live Sync handles both directions. First connection: a new or `pi`-imported project pulls once after Studio opens the place; an existing project runs only `rbx lon`.

```powershell
rbx pl
rbx ps src/ServerScriptService/Main.server.luau   # a push of up to 25 scripts reads them back: `verified: true`
rbx lon
rbx lst
rbx lof
```

`ps` pushes files or directories; for store edits pass the store file with `-i ID` per instance (with its subtree), or `-u` to upsert without deleting; deletions need the store file without `-i`.
A push, `prop` or `del` above `liveSync.changesThreshold` (default 5; 0 = all) applies nothing and returns `heldForReview` with the change list in `review`: check it, then rerun with `--yes`. Script files named on the `ps` command line apply without the hold; `ps -s SERVICE` pushes a service's store file. Editor and Live Sync pushes open a Studio panel instead (`rbx rv apply|skip`). `--no-review` also disables protected writes.
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

A place closed without saving can reopen older than the files: set `rbx cfg set liveSync.initialSyncPriority verify` before `rbx ro` and read `rbx lst --details`.

`rbx lst --details` shows both values and verify paths as `push:` (files differ) or `pull:` (Studio differs); `rbx lon --prefer studio|editor` overrides verify for that run. Keep the side you can account for: `studio` after merely opening the place, `editor` when the files carry your work; ask only when both sides hold edits you did not make.

```powershell
rbx cfg get liveSync.initialSyncPriority
rbx cfg set liveSync.initialSyncPriority reconcile
rbx cfg set liveSync.initialConflictPreference none
```

`reconcile` applies the comparison; `verify` only reports differences and holds pushes and pulls (`pushesPaused` in `rbx status`) until a side is chosen; with no differences the session continues in reconcile.
Conflict preferences: `none`, `studio`, `editor` (project files). A preference resolves ordinary conflicts, never direct PackageLink edits.

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
