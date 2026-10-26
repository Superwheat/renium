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

Start once for sustained editing. Saved files and Studio edits sync both ways. Unsaved buffers do not sync; file edits during Play wait for Edit mode.

Trust successful edits while Live Sync reports no problem. Don't poll, push, reread Studio, or launch Play after every save.
Use `rbx lst --wait 10` only when the next operation needs completed sync, after a reported problem, or when asked.

On failure, inspect `rbx lst --details`, fix the cause, then retry with `rbx rp`. Failed edits stay pending; `rbx dp` discards them.
`lst` restores enabled watchers after a daemon restart; a running daemon alone is no reason to push.

## First connection and conflicts

`no_studio` and `rbx status` include `diagnosis.verdict`. If it says Studio is closed or another place is open and Studio is needed, run `rbx ro`; relay other verdicts unchanged. Do not guess why a place closed.

Live Sync compares each side with their last common Renium state. One-sided changes transfer; independent edits merge; conflicts wait without overwriting either side. Something you added that is now missing from both Studio and the files was deleted in Studio, and the deletion synced; ask before putting it back.

A place closed without saving can reopen older than the files. Before reopening, set `rbx cfg set liveSync.initialSyncPriority verify` or use `rbx lon --initial-sync-priority verify`. Read `rbx lst --details`; if versions differ, ask which to keep and use the returned resolution commands.

The editor asks which version to keep. `rbx lst --details` shows both values and verify paths as `push:` (files differ) or `pull:` (Studio differs). `rbx lon --prefer studio|editor` overrides verify for that run. Choose only with user direction or an existing conflict preference.

```powershell
rbx cfg get liveSync.initialSyncPriority
rbx cfg set liveSync.initialSyncPriority reconcile
rbx cfg set liveSync.initialConflictPreference none
```

Initial modes: `reconcile` applies the comparison; `verify` only reports differences and holds pushes and pulls until a side is chosen (`rbx status` shows `pushesPaused`); when nothing differs the session continues in reconcile, and a later `lon` keeps it there.
Conflict preferences: `none`, `studio`, `editor` (project files). A preference resolves ordinary conflicts, never direct PackageLink edits.

## Manual sync

Without Live Sync, pass the files or directories to `ps`; Renium batches them by service.
For store edits, use returned changed paths and settings IDs (`Service/ID`, since IDs repeat per store) to keep the push scoped.
An unfiltered push reconciles the entire place and can remove Studio-only content.

Pulls restore read-only `.renium` link mirrors from their saved packages within the selected services, including missing targets. `readOnlyLinksRestored` lists them; a difference does not establish which copy is newer. Push the link path to apply the package in Studio, or edit the source and run `lk`. Restore writes do not queue themselves as user edits in Live Sync. `ps --override-packages` deliberately bypasses push protection; its readback keeps that result, but a later pull restores the package again. See [links](links.md) for stray-file handling.

`ps --verify` compares script sources and pushes nothing (`sourceVerifyErrors` lists mismatches); without paths it checks all scripts. It never applies stores or acknowledges a Live Sync push. `ps --verify-sources` pushes, then checks. Don't substitute reads of `Instance.Source`: an open ScriptDocument can differ.

Player capacity (`Players.MaxPlayers`, `PreferredPlayers`) uses Roblox Game Settings. Exports and comparisons preserve these values.

## File-backed undo

Reconciled syncs save their pre-sync state in `.renium/editor-history/sync`.
`rbx rev --sync latest` restores the last sync's affected files; a returned `historyId` selects another. It refuses newer edits and unconfirmed transactions. `rbx rev --path FILE --print` prints a saved copy; a `.renium/editor-history/<id>` path selects an older one.

Live Sync transfers restored files. Otherwise add `--apply-studio`. `--details` lists every restored path. Keep needed history.

## Packages owned by others

Pushes into Roblox packages this account does not own are refused before Studio is touched. Revert to the package's content, or unlink it in Studio (right-click the root, Unlink from Package) to keep an editable copy. Renium never desyncs another creator's package on your behalf.
