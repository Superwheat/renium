# Sync

`pl`: Studio → files. `ps`: files → Studio. Live Sync handles both directions.

```powershell
rbx pl
rbx ps src/ServerScriptService/Main.server.luau --verify
rbx lon
rbx lst
rbx lof
```

Work in the place folder, or put `--place <alias|placeId>` before the command.
Renium manages the connection; no daemon setup is needed.

## Live Sync

Start once for sustained editing. Saved files flow to Studio; Studio edits flow to files. Unsaved editor buffers do not sync. File edits during Play wait for Edit mode.

Trust successful edits while Live Sync reports no problem. Don't poll, push, reread Studio, or launch Play after every save.
Use `rbx lst --wait 10` only when the next operation needs completed sync, after a reported problem, or when asked.

For a failure, inspect `rbx lst --details`, fix the cause, then retry with `rbx rp`.
`rbx dp` discards pending work. Failed edits remain pending until resolved or discarded.
`lst` also restores an enabled watcher after a daemon restart; `daemon.running: true` is not a reason to push manually.

## First connection and conflicts

Live Sync compares each side with their last common Renium state. One-sided changes transfer; independent edits merge; conflicts wait without overwriting either side.

The editor asks which version to keep, then resumes startup. The CLI returns the conflict and resolution commands. Choose only with user direction or an existing conflict preference.

```powershell
rbx cfg get liveSync.initialSyncPriority
rbx cfg set liveSync.initialSyncPriority reconcile
rbx cfg set liveSync.initialConflictPreference none
```

Initial modes: `reconcile` applies the comparison; `verify` only reports differences.
Conflict preferences: `none`, `studio`, `editor` (project files). A preference resolves ordinary conflicts, never direct PackageLink edits.

## Manual sync

Without Live Sync, pass the files or directories to `ps`; Renium batches them by service.
For store edits, use returned changed paths and settings IDs to keep the push scoped.
An unfiltered push reconciles the entire place and can remove Studio-only content.

`ps --verify` checks selected script sources. Don't substitute reads of `Instance.Source`: an open ScriptDocument can differ.

Player capacity (`Players.MaxPlayers` and `PreferredPlayers`) is managed through Roblox Game Settings, not push. Saved exports and file comparisons still preserve/report these values.

## File-backed undo

Reconciled syncs save their pre-sync state in `.renium/editor-history/sync`.
`rbx rev --sync latest` restores the last sync's affected files; use a returned `historyId` instead of `latest` to select one. It refuses to overwrite newer edits or restore an unconfirmed transaction.

Live Sync transfers restored files normally. Without Live Sync, add `--apply-studio`. Use `--details` only when you need every restored path. Don't delete history you still need.
