# Sync

`pl`: Studio → files. `ps`: files → Studio. Live Sync handles both directions.

```powershell
rbx pl
rbx ps src/ServerScriptService/Main.server.luau --verify-sources
rbx lon
rbx lst
rbx lof
```

`ps` pushes files or directories. For store edits pass the store file and `-i ID` per instance, or `-u` to upsert without deleting. An ID selects that instance for properties and its subtree for scripts. Deletions need the store file pushed without `-i`.
Work in the place folder or add `--place <alias|placeId>`.

## Live Sync

Start once for sustained editing. Saved files flow to Studio; Studio edits flow to files. Unsaved editor buffers do not sync. File edits during Play wait for Edit mode.

Trust successful edits while Live Sync reports no problem. Don't poll, push, reread Studio, or launch Play after every save.
Use `rbx lst --wait 10` only when the next operation needs completed sync, after a reported problem, or when asked.

For a failure, inspect `rbx lst --details`, fix the cause, then retry with `rbx rp`.
`rbx dp` discards pending work. Failed edits remain pending until resolved or discarded.
`lst` also restores an enabled watcher after a daemon restart; `daemon.running: true` is not a reason to push manually.

## First connection and conflicts

`no_studio` and `rbx status` include `diagnosis.verdict`. If it says Studio is closed or another place is open and Studio is needed, run `rbx ro`; relay other verdicts unchanged. Do not guess why a place closed.

Live Sync compares each side with their last common Renium state. One-sided changes transfer; independent edits merge; conflicts wait without overwriting either side. Something you added that is now missing from both Studio and the files was deleted in Studio, and the deletion synced; ask before putting it back.

A published place that closed without saving reopens older than the files, and its differences count as Studio edits. Before reopening one, run `rbx cfg set liveSync.initialSyncPriority verify` (or start with `rbx lon --initial-sync-priority verify`), then read `rbx lst --details`: no differences means Studio kept the work. Otherwise ask the user which side to keep and use the resolution commands the CLI returns.

The editor asks which version to keep; the CLI returns resolution commands. `rbx lst --details` lists conflicts with both values and verify paths as `push:` (files differ) or `pull:` (Studio differs). `rbx lon --prefer studio|editor` reconciles for that run even when the configured mode is verify. Choose only with user direction or an existing conflict preference.

```powershell
rbx cfg get liveSync.initialSyncPriority
rbx cfg set liveSync.initialSyncPriority reconcile
rbx cfg set liveSync.initialConflictPreference none
```

Initial modes: `reconcile` applies the comparison; `verify` only reports differences and holds pushes and pulls until a side is chosen (`rbx status` shows `pushesPaused`); when nothing differs the session continues in reconcile, and a later `lon` keeps it there.
Conflict preferences: `none`, `studio`, `editor` (project files). A preference resolves ordinary conflicts, never direct PackageLink edits.

## Manual sync

Without Live Sync, pass the files or directories to `ps`; Renium batches them by service.
For store edits, use returned changed paths and settings IDs to keep the push scoped.
An unfiltered push reconciles the entire place and can remove Studio-only content.

`ps --verify` compares script sources and pushes nothing (`sourceVerifyErrors` lists mismatches); without paths it checks all scripts. It never applies stores or acknowledges a Live Sync push. `ps --verify-sources` pushes, then checks. Don't substitute reads of `Instance.Source`: an open ScriptDocument can differ.

Player capacity (`Players.MaxPlayers` and `PreferredPlayers`) is managed through Roblox Game Settings, not push. Saved exports and file comparisons still preserve/report these values.

## File-backed undo

Reconciled syncs save their pre-sync state in `.renium/editor-history/sync`.
`rbx rev --sync latest` restores the last sync's affected files; use a returned `historyId` instead of `latest` to select one. It refuses to overwrite newer edits or restore an unconfirmed transaction. `rbx rev --path FILE --print` writes a file's saved copy to stdout instead of restoring it; pass a `.renium/editor-history/<id>` entry as `--path` to pick an older copy.

Live Sync transfers restored files normally. Without Live Sync, add `--apply-studio`. Use `--details` only when you need every restored path. Don't delete history you still need.

## Packages owned by others

Pushes inside Roblox packages this account does not own are refused before Studio is touched, naming the package and creator. Revert those files to the package's content, or unlink the package in Studio (right-click the package root, Unlink from Package) when the project is meant to carry its own copy; Renium never desyncs or rewrites someone else's package on your behalf.
