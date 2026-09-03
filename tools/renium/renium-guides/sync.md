# Sync

`pl` is Studio → files; `ps` is files → Studio. Live Sync uses one shared connection.

```powershell
rbx pl
rbx ps src/StarterGui/AuditClient.client.luau
rbx lon
rbx lst
rbx lst --wait 10
rbx lof
```

Use `--verify` for exact script pushes. Don't verify through `Instance.Source`; an open ScriptDocument may differ.

Run from the place folder. At a multi-place experience root, put `--place <alias|placeId>` before the command. Renium handles the daemon and runtime.

For sustained edits, start Live Sync once. Saved file changes go to Studio and Studio changes go to project files; unsaved editor buffers aren't visible to Renium. Failed edits stay pending. After fixing the cause, use `rbx rp`; use `rbx dp` only to discard them.

On first connection, Live Sync compares Studio and the project against their last common Renium state. Independent edits are merged. Conflicting edits remain pending without changing either side. `reconcile` is the default; `verify` only reports differences. An optional Studio or editor preference resolves ordinary conflicts, but never direct PackageLink edits.

The editor asks which side to keep only when both sides changed the same content, then finishes starting Live Sync automatically. `rbx lon` reports the conflict and both commands that resolve it; no Studio inspection is needed.

```powershell
rbx cfg get liveSync.initialSyncPriority
rbx cfg set liveSync.initialSyncPriority reconcile
rbx cfg set liveSync.initialConflictPreference none
```

Conflict preferences are `none`, `studio`, and `editor` (project files).

`rbx lst` restores an enabled watcher after a daemon restart. If `daemon.running` is true, don't repeat edits with a manual push.

`rbx lst --wait 10` waits up to 10 seconds for file edits to finish syncing.

Status is compact by default. Add `--details` only when compact status reports a pending change, conflict, or failure that needs diagnosis.

Without Live Sync, list files or directories after `rbx ps`. Renium expands directories and batches services. `--verify` checks selected scripts. Use an unfiltered push only to replace the full place.
