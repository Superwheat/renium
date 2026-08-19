# Sync

Read `RENIUM.md` first. `pull` means Studio to files. `push` means files to Studio. The editor's Live Sync command runs both directions.

```powershell
rbx pull
rbx push --changed-path src/StarterGui/AuditClient.client.luau --no-review --yes
```

Add `--verify` when checking an exact script push. Don't verify it through `Instance.Source` in `rbx l`; an open ScriptDocument can hold the current editor source separately.

Run these from the active place folder. At an experience root with more than one place, add the global `--place <alias|placeId>` selector before the command. Renium starts or reuses the shared daemon and waits for the matching Studio runtime.

The CLI doesn't watch files. After project edits, pass each returned `changedPaths` entry to `rbx push --changed-path`; repeat the flag for multiple files. After Studio edits, pull. Use an unfiltered push only for a full place replacement. Use `rbx st --no-start` to read live-sync state and `rbx st --clear-pending` only to discard queued Studio changes.
