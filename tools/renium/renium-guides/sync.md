# Sync

Read `RENIUM.md` first. `pull` means Studio to files. `push` means files to Studio. Live sync runs both directions in the shared Renium daemon, so Cursor and agents reuse one watcher and one Studio connection.

```powershell
rbx pull
rbx push --changed-path src/StarterGui/AuditClient.client.luau --no-review --yes
rbx live-start
rbx live-status
rbx live-stop
```

Add `--verify` when checking an exact script push. Don't verify it through `Instance.Source` in `rbx l`; an open ScriptDocument can hold the current editor source separately.

Run these from the active place folder. At an experience root with more than one place, add the global `--place <alias|placeId>` selector before the command. Renium starts or reuses the shared daemon and waits for the matching Studio runtime.

For sustained edits, start live sync once and edit files normally. Renium batches nearby file saves, verifies script writes, retries one transient transport failure, and keeps permanent failures pending. Use `rbx retry-pending` after fixing the cause or `rbx discard-pending` only when those pending file edits should not reach Studio.

Use a one-off filtered push when live sync isn't running. Repeat `--changed-path` for multiple files; Renium handles one batch across services. Use an unfiltered push only for an intentional full place replacement.
